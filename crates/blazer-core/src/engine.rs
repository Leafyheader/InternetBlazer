//! The public engine API: adding, starting, pausing and observing downloads, and managing queues
//! and speed limits.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    checksum, download,
    error::{Error, Result},
    filename, fsio,
    limit::{self, RateLimiter},
    media, playlist, probe,
    queue::{self, Queue},
    store::{RedbStore, Store},
    tools,
    types::{
        self, CheckState, Checksum, DownloadId, DownloadRecord, Event, LinkInfo, MediaFormat,
        MediaInfo, MediaTools, QueueId, Retry, Settings, Status, Tool,
    },
};

pub const MAX_CONNECTIONS: usize = 32;
pub const MAX_AUTO_RETRIES: u32 = 100;
const EVENT_CAPACITY: usize = 1024;
/// How long what yt-dlp found on a page is reused, so the add dialog and adding the download don't
/// ask it again right after the browser did.
const MEDIA_CACHE_TIME: Duration = Duration::from_secs(5 * 60);
const MEDIA_CACHE_SIZE: usize = 8;

#[derive(Debug, Clone)]
pub struct Config {
    /// Parallel connections per download, at most [`MAX_CONNECTIONS`].
    pub connections: usize,
    /// A segment is only split when both halves would be at least this many bytes.
    pub min_split_size: u64,
    /// Attempts per connection after consecutive failures without progress.
    pub max_retries: u32,
    /// Base delay between retries, doubled on each consecutive failure.
    pub retry_delay: Duration,
    /// Times a download that failed for a temporary reason, such as a lost connection, is tried
    /// again on its own before it counts as failed.
    pub auto_retries: u32,
    /// Wait before the first automatic retry, doubled for each one after it.
    pub auto_retry_delay: Duration,
    pub max_auto_retry_delay: Duration,
    pub connect_timeout: Duration,
    /// Drops a connection that receives nothing for this long.
    pub read_timeout: Duration,
    /// Progress reports a download as stalled once nothing has arrived for this long.
    pub stall_after: Duration,
    /// Bytes each connection buffers before writing to disk.
    pub write_buffer: usize,
    pub progress_interval: Duration,
    /// How often progress is flushed to disk and saved to the store.
    pub checkpoint_interval: Duration,
    pub user_agent: String,
    /// Where [`Engine::install_ytdlp`] puts yt-dlp, and where yt-dlp and ffmpeg are looked for
    /// before `PATH`. Without it, yt-dlp cannot be installed.
    pub tools_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connections: 8,
            min_split_size: 1024 * 1024,
            max_retries: 8,
            retry_delay: Duration::from_secs(1),
            // About an hour of retries in all: enough for a router restart or a laptop's sleep.
            auto_retries: 10,
            auto_retry_delay: Duration::from_secs(15),
            max_auto_retry_delay: Duration::from_secs(10 * 60),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            stall_after: Duration::from_secs(3),
            write_buffer: 512 * 1024,
            progress_interval: Duration::from_millis(250),
            checkpoint_interval: Duration::from_secs(1),
            user_agent: concat!("InternetBlazer/", env!("CARGO_PKG_VERSION")).to_string(),
            tools_dir: None,
        }
    }
}

/// A request to add a download.
#[derive(Debug, Clone)]
pub struct NewDownload {
    pub url: String,
    pub dir: PathBuf,
    /// Overrides the name suggested by the server.
    pub file_name: Option<String>,
    pub connections: Option<usize>,
    pub headers: Vec<(String, String)>,
    /// Defaults to [`QueueId::MAIN`].
    pub queue: Option<QueueId>,
    /// Bytes per second.
    pub speed_limit: Option<u64>,
    /// Downloads the video or sound on the page at `url` with yt-dlp, instead of the file there.
    /// For an HLS or DASH playlist, which the engine read itself, the engine fetches its pieces.
    pub media: Option<MediaFormat>,
    /// Queue it to start instead of adding it paused.
    pub start: bool,
    /// The SHA-256 the file should have, checked once it finishes. Any text holding one will do;
    /// see [`parse_sha256`](crate::parse_sha256).
    pub sha256: Option<String>,
}

impl NewDownload {
    pub fn new(url: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            dir: dir.into(),
            file_name: None,
            connections: None,
            headers: Vec::new(),
            queue: None,
            speed_limit: None,
            media: None,
            start: true,
            sha256: None,
        }
    }
}

/// Cheap to clone; all clones share the same downloads.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) client: reqwest::Client,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) config: Config,
    events: broadcast::Sender<Event>,
    active: Mutex<HashMap<DownloadId, Active>>,
    tasks: TaskTracker,
    /// Cancelled on shutdown; the parent of every running download's token.
    pub(crate) closed: CancellationToken,
    /// Held while deciding what runs, so that starting, pausing and finishing downloads see each
    /// other's status changes. Taken before `records` and `active`.
    sched: Mutex<()>,
    /// Held for read-modify-write updates of stored records.
    records: Mutex<()>,
    /// The global limit, shared by every connection of every download.
    pub(crate) speed_limit: RateLimiter,
    /// The stored settings. Held while they are saved, so concurrent changes don't undo each other.
    settings: Mutex<Settings>,
    /// Held while yt-dlp is installed, so two installs don't replace each other's files.
    installing: tokio::sync::Mutex<()>,
    /// Downloads whose files are being moved, which must not start until they are in place.
    held: Mutex<HashSet<DownloadId>>,
    /// Held while a new download picks its file name and is saved, so downloads added at the
    /// same time don't take the same name.
    adding: Mutex<()>,
    /// Pages yt-dlp recently looked at, newest last.
    media_cache: Mutex<Vec<FoundMedia>>,
}

/// What yt-dlp found on a page.
struct FoundMedia {
    url: String,
    at: Instant,
    info: MediaInfo,
    /// Found only without the browser's cookies, so the download must leave them out too.
    without_cookies: bool,
}

fn is_cookie(header: &(String, String)) -> bool {
    header.0.eq_ignore_ascii_case("cookie")
}

/// A running download.
struct Active {
    cancel: CancellationToken,
    queue: QueueId,
    limiter: Arc<RateLimiter>,
}

impl Inner {
    pub(crate) fn emit(&self, event: Event) {
        // Sending only fails when nobody is subscribed.
        let _ = self.events.send(event);
    }

    /// Connections for a download that doesn't ask for a number itself.
    pub(crate) fn connections(&self) -> usize {
        self.settings.lock().connections.unwrap_or(self.config.connections)
    }

    pub(crate) fn auto_retries(&self) -> u32 {
        self.settings.lock().auto_retries.unwrap_or(self.config.auto_retries)
    }

    pub(crate) fn deactivate(&self, id: DownloadId) {
        let _sched = self.sched.lock();
        self.active.lock().remove(&id);
    }

    /// Saves the final record of a download that stopped running and marks it inactive, in one
    /// step as seen by anyone holding `sched`.
    pub(crate) fn finish(&self, record: &mut DownloadRecord) -> Result<()> {
        let _sched = self.sched.lock();
        let result = self.save_running(record);
        self.active.lock().remove(&record.id);
        result
    }

    /// Saves the record of a running download, keeping the stored values of the fields that can
    /// change while it runs.
    pub(crate) fn save_running(&self, record: &mut DownloadRecord) -> Result<()> {
        let _records = self.records.lock();
        if let Some(stored) = self.store.get(record.id)? {
            record.queue = stored.queue;
            record.speed_limit = stored.speed_limit;
            record.checksum = stored.checksum;
        }
        // Saved as under way, so that waiting for the download to finish also finds the check
        // started; the caller starts it with `run_check`.
        if record.status == Status::Completed
            && let Some(checksum) = &mut record.checksum
        {
            checksum.state = CheckState::Checking;
        }
        self.store.save(record)
    }

    /// Checks the finished file of a download against its SHA-256, unless a check is under way.
    pub(crate) fn start_check(self: &Arc<Self>, id: DownloadId) -> Result<()> {
        let mut started = false;
        let record = self.update(id, |r| {
            if r.status == Status::Completed
                && let Some(checksum) = &mut r.checksum
                && checksum.state != CheckState::Checking
            {
                checksum.state = CheckState::Checking;
                started = true;
            }
        })?;
        if started {
            self.emit(Event::ChecksumChanged { id, checksum: record.checksum });
            self.run_check(id);
        }
        Ok(())
    }

    /// Hashes the file of a download whose check is saved as under way, and saves what it found.
    pub(crate) fn run_check(self: &Arc<Self>, id: DownloadId) {
        let inner = self.clone();
        self.tasks.spawn(async move {
            let path = match inner.store.get(id) {
                Ok(Some(record)) => record.path(),
                Ok(None) => return,
                Err(e) => {
                    tracing::error!(%id, "failed to load download: {e}");
                    return;
                }
            };
            let cancel = inner.closed.clone();
            let hashed =
                tokio::task::spawn_blocking(move || checksum::hash_file(&path, &cancel)).await;
            let found = match hashed {
                Ok(Ok(Some(actual))) => Ok(actual),
                // Shutting down; it is checked again when the app next starts.
                Ok(Ok(None)) => return,
                Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
                    Err("the file isn't there any more".to_string())
                }
                Ok(Err(e)) => Err(format!("couldn't read the file: {e}")),
                Err(e) => Err(e.to_string()),
            };
            let result = inner.update(id, |r| {
                // Restarted, or given another hash that is checked anew, in the meantime.
                if r.status != Status::Completed {
                    return;
                }
                if let Some(checksum) = &mut r.checksum
                    && checksum.state == CheckState::Checking
                {
                    checksum.state = match &found {
                        Ok(actual) if *actual == checksum.expected => CheckState::Matched,
                        Ok(actual) => CheckState::Mismatched { actual: actual.clone() },
                        Err(error) => CheckState::Failed { error: error.clone() },
                    };
                }
            });
            match result {
                Ok(record) => {
                    if let Some(checksum) = &record.checksum {
                        tracing::info!(%id, "checked SHA-256: {:?}", checksum.state);
                    }
                    inner.emit(Event::ChecksumChanged { id, checksum: record.checksum });
                }
                Err(Error::NotFound(_)) => {}
                Err(e) => tracing::error!(%id, "failed to save the SHA-256 check: {e}"),
            }
        });
    }

    fn update(
        &self,
        id: DownloadId,
        change: impl FnOnce(&mut DownloadRecord),
    ) -> Result<DownloadRecord> {
        let _records = self.records.lock();
        let mut record = self.store.get(id)?.ok_or(Error::NotFound(id))?;
        change(&mut record);
        self.store.save(&record)?;
        Ok(record)
    }

    /// Queues or pauses a download that is not running, as someone asked. The caller holds
    /// `sched`.
    fn set_status(&self, id: DownloadId, status: Status) -> Result<()> {
        self.update(id, |r| {
            r.status = status;
            r.error = None;
            r.retry = None;
            r.interrupted = false;
        })?;
        self.emit(Event::StatusChanged { id, status, error: None });
        Ok(())
    }

    /// Queues the download again after `delay`, unless it was paused, started or removed in the
    /// meantime.
    pub(crate) fn retry_later(self: &Arc<Self>, id: DownloadId, retry: Retry, delay: Duration) {
        let inner = self.clone();
        self.tasks.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                // It is picked up again with the other interrupted downloads.
                () = inner.closed.cancelled() => return,
            }
            let result = {
                let _sched = inner.sched.lock();
                inner.update(id, |r| {
                    if r.status == Status::Retrying && r.retry == Some(retry) {
                        r.status = Status::Queued;
                    }
                })
            };
            match result {
                Ok(r) if r.status == Status::Queued => {
                    inner.emit(Event::StatusChanged { id, status: r.status, error: r.error });
                    inner.dispatch();
                }
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(e) => tracing::error!(%id, "failed to retry download: {e}"),
            }
        });
    }

    /// Starts queued downloads, oldest first, while their queues have free slots.
    pub(crate) fn dispatch(self: &Arc<Self>) {
        if let Err(e) = self.try_dispatch() {
            tracing::error!("failed to start queued downloads: {e}");
        }
    }

    fn try_dispatch(self: &Arc<Self>) -> Result<()> {
        let _sched = self.sched.lock();
        if self.closed.is_cancelled() {
            return Ok(());
        }
        let limits: HashMap<QueueId, usize> =
            self.store.queues()?.into_iter().map(|q| (q.id, q.max_active)).collect();
        let records = self.store.list()?;
        let mut active = self.active.lock();
        let mut running: HashMap<QueueId, usize> = HashMap::new();
        for a in active.values() {
            *running.entry(a.queue).or_default() += 1;
        }
        for record in records {
            if record.status != Status::Queued || active.contains_key(&record.id) {
                continue;
            }
            let limit = limits.get(&record.queue).copied().unwrap_or(1);
            let count = running.entry(record.queue).or_default();
            if *count >= limit {
                continue;
            }
            *count += 1;
            let cancel = self.closed.child_token();
            let limiter = Arc::new(RateLimiter::new(record.speed_limit));
            active.insert(
                record.id,
                Active { cancel: cancel.clone(), queue: record.queue, limiter: limiter.clone() },
            );
            self.tasks.spawn(download::run(self.clone(), record.id, cancel, limiter));
        }
        Ok(())
    }
}

impl Engine {
    /// Opens the engine with its state in a redb database at `path`.
    pub fn open(path: impl AsRef<std::path::Path>, config: Config) -> Result<Self> {
        Self::with_store(Arc::new(RedbStore::open(path)?), config)
    }

    pub fn with_store(store: Arc<dyn Store>, config: Config) -> Result<Self> {
        for mut record in store.list()? {
            // A check cut off by quitting is done again; see `resume_interrupted`.
            let mut changed = false;
            if let Some(checksum) = &mut record.checksum
                && checksum.state == CheckState::Checking
            {
                checksum.state = CheckState::Waiting;
                changed = true;
            }
            if record.status != Status::Completed && finished_on_disk(&record) {
                tracing::info!(id = %record.id, "found finished download; marking it complete");
                record.status = Status::Completed;
                record.completed_at = Some(types::now());
                record.error = None;
                record.retry = None;
                record.interrupted = false;
            } else if matches!(
                record.status,
                Status::Downloading | Status::Queued | Status::Retrying
            ) {
                // Cut off by quitting, a crash or a power loss. Paused, so opening the engine
                // (say, to list downloads) never starts any; see `resume_interrupted`.
                record.status = Status::Paused;
                record.retry = None;
                record.interrupted = true;
            } else if !changed {
                continue;
            }
            store.save(&record)?;
        }
        if !store.queues()?.iter().any(|q| q.id == QueueId::MAIN) {
            store.save_queue(&Queue::main())?;
        }
        let settings = store.settings()?;
        let speed_limit = RateLimiter::new(settings.speed_limit);
        let client = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                store,
                config,
                events,
                active: Mutex::default(),
                tasks: TaskTracker::new(),
                closed: CancellationToken::new(),
                sched: Mutex::default(),
                records: Mutex::default(),
                speed_limit,
                settings: Mutex::new(settings),
                installing: tokio::sync::Mutex::default(),
                held: Mutex::default(),
                adding: Mutex::default(),
                media_cache: Mutex::default(),
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn list(&self) -> Result<Vec<DownloadRecord>> {
        self.inner.store.list()
    }

    pub fn get(&self, id: DownloadId) -> Result<DownloadRecord> {
        self.inner.store.get(id)?.ok_or(Error::NotFound(id))
    }

    pub fn is_active(&self, id: DownloadId) -> bool {
        self.inner.active.lock().contains_key(&id)
    }

    /// Finds out what a link points to, such as its size, without adding a download.
    pub async fn inspect(&self, url: &str, headers: &[(String, String)]) -> Result<LinkInfo> {
        let url = parse_url(url)?;
        let info = probe::probe(&self.inner.client, url.as_str(), headers).await?;
        Ok(LinkInfo {
            file_name: info.file_name.map(|name| filename::sanitize(&name)),
            total: info.total,
            resumable: info.resumable,
            page: info.html,
        })
    }

    /// Finds the videos or sounds yt-dlp can download from the page at `url`. An HLS or DASH
    /// playlist is read directly, which takes a moment instead of seconds. What it found is
    /// reused for a few minutes.
    pub async fn inspect_media(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<MediaInfo> {
        self.find_media(url, headers, false).await
    }

    /// Like [`Engine::inspect_media`], for a URL that a video player fetched as a playlist,
    /// though it may not end in `.m3u8` or `.mpd`.
    pub async fn inspect_stream(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<MediaInfo> {
        self.find_media(url, headers, true).await
    }

    async fn find_media(
        &self,
        url: &str,
        headers: &[(String, String)],
        stream: bool,
    ) -> Result<MediaInfo> {
        let url = parse_url(url)?;
        let dir = self.inner.config.tools_dir.as_deref();
        let ffmpeg = tools::ffmpeg(dir).is_some();
        if let Some(found) = self.found_media(url.as_str(), ffmpeg) {
            return Ok(found.0);
        }
        let read = if stream || playlist::looks_like_playlist(&url) {
            match playlist::inspect(&self.inner.client, &url, headers, ffmpeg).await {
                Err(Error::NotPlaylist) => None,
                result => Some(result?),
            }
        } else {
            None
        };
        let mut without_cookies = false;
        let info = match read {
            Some(info) => info,
            None => self.ask_ytdlp(&url, headers, ffmpeg, &mut without_cookies).await?,
        };
        let mut cache = self.inner.media_cache.lock();
        cache.retain(|f| f.url != url.as_str());
        if cache.len() >= MEDIA_CACHE_SIZE {
            cache.remove(0);
        }
        cache.push(FoundMedia {
            url: url.to_string(),
            at: Instant::now(),
            info: info.clone(),
            without_cookies,
        });
        Ok(info)
    }

    async fn ask_ytdlp(
        &self,
        url: &url::Url,
        headers: &[(String, String)],
        ffmpeg: bool,
        without_cookies: &mut bool,
    ) -> Result<MediaInfo> {
        let dir = self.inner.config.tools_dir.as_deref();
        let (ytdlp, _) = tools::ytdlp(dir).ok_or(Error::NoYtdlp)?;
        match media::inspect(&ytdlp, dir, ffmpeg, url.as_str(), headers).await {
            // Signed in, YouTube serves yt-dlp in ways it can only read with a JavaScript
            // runtime, and without one it finds no formats at all. Signed out it finds them.
            Err(Error::Media { message, .. })
                if message.contains("Requested format is not available")
                    && headers.iter().any(is_cookie) =>
            {
                tracing::info!("yt-dlp found no formats with cookies; trying without: {url}");
                *without_cookies = true;
                let headers: Vec<_> = headers.iter().filter(|h| !is_cookie(h)).cloned().collect();
                media::inspect(&ytdlp, dir, ffmpeg, url.as_str(), &headers).await
            }
            result => result,
        }
    }

    /// What yt-dlp found at `url` a short while ago, and whether that was without cookies.
    fn found_media(&self, url: &str, ffmpeg: bool) -> Option<(MediaInfo, bool)> {
        let mut cache = self.inner.media_cache.lock();
        cache.retain(|f| f.at.elapsed() < MEDIA_CACHE_TIME);
        // Installing ffmpeg since changes what can be offered.
        cache
            .iter()
            .find(|f| f.url == url && f.info.ffmpeg == ffmpeg)
            .map(|f| (f.info.clone(), f.without_cookies))
    }

    /// The yt-dlp, deno and ffmpeg that media downloads would use.
    pub async fn media_tools(&self) -> MediaTools {
        let dir = self.inner.config.tools_dir.as_deref();
        let (ytdlp, deno) = tokio::join!(tool(tools::ytdlp(dir)), tool(tools::deno(dir)));
        MediaTools { ytdlp, deno, ffmpeg: tools::ffmpeg(dir) }
    }

    /// Whether the yt-dlp this engine installed is the single-file build, which takes seconds
    /// to start each time. [`Engine::install_ytdlp`] replaces it with the quick one.
    pub fn ytdlp_is_slow(&self) -> bool {
        tools::ytdlp_is_single_file(self.inner.config.tools_dir.as_deref())
    }

    /// Installs the latest yt-dlp in [`Config::tools_dir`], or updates the copy there. Running
    /// downloads keep the copy they started with.
    pub async fn install_ytdlp(&self) -> Result<Tool> {
        let Some(dir) = self.inner.config.tools_dir.as_deref() else {
            return Err(Error::Invalid("no folder was set up for yt-dlp".into()));
        };
        let _installing = self.inner.installing.lock().await;
        let path = tools::install_ytdlp(&self.inner.client, dir).await?;
        let version = tools::version(&path).await?;
        tracing::info!("installed yt-dlp {version}");
        Ok(Tool { path, version, managed: true })
    }

    /// Installs the latest deno in [`Config::tools_dir`], or updates the copy there.
    pub async fn install_deno(&self) -> Result<Tool> {
        let Some(dir) = self.inner.config.tools_dir.as_deref() else {
            return Err(Error::Invalid("no folder was set up for deno".into()));
        };
        let _installing = self.inner.installing.lock().await;
        let path = tools::install_deno(&self.inner.client, dir).await?;
        let version = tools::version(&path).await?;
        tracing::info!("installed {version}");
        Ok(Tool { path, version: short_version(&version), managed: true })
    }

    /// Probes the URL, picks a unique file name and records the download.
    pub async fn add(&self, mut new: NewDownload) -> Result<DownloadId> {
        let queue = new.queue.unwrap_or(QueueId::MAIN);
        self.queue(queue)?;
        let url = parse_url(&new.url)?;
        let sha256 = new.sha256.as_deref().map(parse_checksum).transpose()?;
        if new.media.is_some() {
            let ffmpeg = tools::ffmpeg(self.inner.config.tools_dir.as_deref()).is_some();
            if let Some((_, true)) = self.found_media(url.as_str(), ffmpeg) {
                new.headers.retain(|h| !is_cookie(h));
            }
        }
        let info = match new.media {
            // yt-dlp finds the file each time it runs; there is nothing to probe.
            Some(_) => probe::ProbeInfo { resumable: true, ..Default::default() },
            None => probe::probe(&self.inner.client, url.as_str(), &new.headers).await?,
        };

        let name = match (new.media, new.file_name.as_deref()) {
            (_, Some(name)) => filename::sanitize(name),
            (Some(format), None) => {
                let found = self.inspect_media(url.as_str(), &new.headers).await?;
                media::guess_name(&found, format)
            }
            (None, None) => filename::sanitize(info.file_name.as_deref().unwrap_or("download")),
        };
        // A playlist the engine read itself, rather than a page yt-dlp looked at.
        let stream = new.media.is_some() && {
            let ffmpeg = tools::ffmpeg(self.inner.config.tools_dir.as_deref()).is_some();
            match self.found_media(url.as_str(), ffmpeg) {
                Some((found, _)) => matches!(found.site.as_str(), "HLS" | "DASH"),
                None => playlist::looks_like_playlist(&url),
            }
        };
        let adding = self.inner.adding.lock();
        let in_progress: Vec<PathBuf> = self
            .list()?
            .into_iter()
            .filter(|r| r.status != Status::Completed)
            .map(|r| r.path())
            .collect();
        let file_name = match new.media {
            // yt-dlp names its files after the stem and may choose another extension, so no
            // other file may share the stem.
            Some(_) => filename::unique(&new.dir, &name, |path| {
                media::stem_taken(path)
                    || in_progress
                        .iter()
                        .any(|p| p.parent() == path.parent() && media::same_stem(p, path))
            }),
            None => filename::unique(&new.dir, &name, |path| {
                path.exists()
                    || filename::part_path(path).exists()
                    || in_progress.iter().any(|p| p == path)
            }),
        };

        let id = self.inner.store.next_id()?;
        let record = DownloadRecord {
            id,
            url: url.into(),
            headers: new.headers,
            dir: new.dir,
            file_name,
            total: info.total,
            resumable: info.resumable,
            etag: info.etag,
            last_modified: info.last_modified,
            connections: new.connections,
            queue,
            speed_limit: limit::normalize(new.speed_limit),
            media: new.media,
            stream,
            tracks: Vec::new(),
            segments: Vec::new(),
            status: if new.start { Status::Queued } else { Status::Paused },
            error: None,
            retry: None,
            interrupted: false,
            added_at: types::now(),
            completed_at: None,
            last_try_at: None,
            checksum: sha256.map(|expected| Checksum { expected, state: CheckState::Waiting }),
        };
        self.inner.store.save(&record)?;
        drop(adding);
        self.inner.emit(Event::Added(record));
        if new.start {
            self.inner.dispatch();
        }
        Ok(id)
    }

    /// Queues a paused or failed download, or one waiting to retry. It starts as soon as its
    /// queue has a free slot.
    pub fn start(&self, id: DownloadId) -> Result<()> {
        self.get(id)?;
        if self.inner.held.lock().contains(&id) {
            return Err(Error::Invalid(
                "the download is being moved; start it once that's done".into(),
            ));
        }
        self.enqueue(|r| r.id == id)
    }

    /// Asks a running download to stop after saving its progress, or takes a queued one out of
    /// its queue. Use [`Engine::wait`] to know when it has stopped.
    pub fn pause(&self, id: DownloadId) {
        self.pause_where(|r| r.id == id);
    }

    /// Queues every paused or failed download in the queue.
    pub fn start_queue(&self, queue: QueueId) -> Result<()> {
        self.queue(queue)?;
        self.enqueue(|r| r.queue == queue)
    }

    /// Pauses every running or queued download in the queue.
    pub fn stop_queue(&self, queue: QueueId) -> Result<()> {
        self.queue(queue)?;
        self.pause_where(|r| r.queue == queue);
        Ok(())
    }

    fn enqueue(&self, pick: impl Fn(&DownloadRecord) -> bool) -> Result<()> {
        {
            let _sched = self.inner.sched.lock();
            for record in self.list()? {
                if matches!(record.status, Status::Paused | Status::Failed | Status::Retrying)
                    && !self.is_active(record.id)
                    && !self.inner.held.lock().contains(&record.id)
                    && pick(&record)
                {
                    self.inner.set_status(record.id, Status::Queued)?;
                }
            }
        }
        self.inner.dispatch();
        Ok(())
    }

    fn pause_where(&self, pick: impl Fn(&DownloadRecord) -> bool) {
        let _sched = self.inner.sched.lock();
        let records = match self.list() {
            Ok(records) => records,
            Err(e) => return tracing::error!("failed to pause downloads: {e}"),
        };
        for record in records.iter().filter(|r| pick(r)) {
            if let Some(active) = self.inner.active.lock().get(&record.id) {
                active.cancel.cancel();
            } else if matches!(record.status, Status::Queued | Status::Retrying)
                && let Err(e) = self.inner.set_status(record.id, Status::Paused)
            {
                tracing::error!(id = %record.id, "failed to pause download: {e}");
            }
        }
    }

    /// Queues the downloads that were running or waiting to run when the engine last stopped,
    /// such as when the app quit or crashed, and checks the finished files whose SHA-256 checks
    /// were cut off. Meant for the desktop app as it starts; a tool that only lists downloads
    /// should not call it.
    pub fn resume_interrupted(&self) -> Result<()> {
        for record in self.list()? {
            if record.status == Status::Completed
                && record.checksum.as_ref().is_some_and(|c| c.state == CheckState::Waiting)
            {
                self.inner.start_check(record.id)?;
            }
        }
        self.enqueue(|r| r.interrupted)
    }

    /// Waits until the download is not running, and returns its final record. One waiting to
    /// retry counts as not running.
    pub async fn wait(&self, id: DownloadId) -> Result<DownloadRecord> {
        let mut events = self.subscribe();
        // A finishing download deactivates before it announces its new status, so every pass
        // either sees it inactive or has an event still to come.
        while self.is_active(id) {
            if let Err(RecvError::Closed) = events.recv().await {
                break;
            }
        }
        self.get(id)
    }

    /// Downloads the file again from scratch, for example after the server copy changed.
    pub async fn restart(&self, id: DownloadId) -> Result<()> {
        self.pause(id);
        let record = self.wait(id).await?;
        let info = match record.media {
            Some(_) => {
                for path in media::working_files(&record) {
                    remove_if_exists(&path)?;
                }
                probe::ProbeInfo { resumable: true, ..Default::default() }
            }
            None => probe::probe(&self.inner.client, &record.url, &record.headers).await?,
        };
        {
            let _sched = self.inner.sched.lock();
            self.inner.update(id, |r| {
                r.total = info.total;
                r.resumable = info.resumable;
                r.etag = info.etag;
                r.last_modified = info.last_modified;
                r.segments.clear();
                r.tracks.clear();
                r.status = Status::Queued;
                r.error = None;
                r.retry = None;
                r.interrupted = false;
                r.completed_at = None;
                if let Some(checksum) = &mut r.checksum {
                    checksum.state = CheckState::Waiting;
                }
            })?;
        }
        self.inner.emit(Event::StatusChanged { id, status: Status::Queued, error: None });
        self.inner.dispatch();
        Ok(())
    }

    /// Points a download that is not running at a new address, such as a fresh link after the
    /// old one expired. Progress is kept, so once some of the file is downloaded the new address
    /// must serve the same file: the same size and version, with resume support.
    pub async fn set_url(&self, id: DownloadId, url: &str) -> Result<()> {
        let url = parse_url(url)?;
        let record = self.get(id)?;
        let info = match record.media {
            // yt-dlp looks at the page again each time it runs.
            Some(_) => probe::ProbeInfo { resumable: true, ..Default::default() },
            None => probe::probe(&self.inner.client, url.as_str(), &record.headers).await?,
        };
        // yt-dlp's partial files belong to the video rather than the address, so they are kept.
        let started = record.media.is_none() && record.downloaded() > 0;
        if started {
            let size = |n: Option<u64>| n.map_or("unknown".into(), crate::fsio::format_bytes);
            if info.total != record.total {
                return Err(Error::Invalid(format!(
                    "the new address has a different file: its size is {}, not {}",
                    size(info.total),
                    size(record.total)
                )));
            }
            if !info.matches(record.etag.as_deref(), record.last_modified.as_deref()) {
                return Err(Error::Invalid(
                    "the new address has a different version of the file".into(),
                ));
            }
            if !info.resumable {
                return Err(Error::Invalid(
                    "the new address cannot resume downloads, so the progress would be lost; \
                     restart the download instead"
                        .into(),
                ));
            }
        }
        let record = {
            let _sched = self.inner.sched.lock();
            if self.is_active(id) {
                return Err(Error::Invalid(
                    "pause the download before changing its address".into(),
                ));
            }
            if self.get(id)?.status == Status::Completed {
                return Err(Error::Invalid("the download has already finished".into()));
            }
            self.inner.update(id, |r| {
                r.url = url.into();
                r.error = None;
                if !started {
                    r.total = info.total;
                    r.resumable = info.resumable;
                    r.etag = info.etag;
                    r.last_modified = info.last_modified;
                    r.segments.clear();
                }
            })?
        };
        self.inner.emit(Event::Updated(record));
        Ok(())
    }

    /// Moves or renames a download that is not running: the finished file, or the partial data a
    /// paused or failed download carries on from. Never replaces another file.
    pub async fn move_to(&self, id: DownloadId, dir: PathBuf, file_name: &str) -> Result<()> {
        let name = filename::sanitize(file_name);
        let moves = {
            let _sched = self.inner.sched.lock();
            let record = self.get(id)?;
            if self.is_active(id) || matches!(record.status, Status::Queued | Status::Retrying) {
                return Err(Error::Invalid("pause the download before moving it".into()));
            }
            let target =
                DownloadRecord { dir: dir.clone(), file_name: name.clone(), ..record.clone() };
            let moves = planned_moves(&record, &target)?;
            if !self.inner.held.lock().insert(id) {
                return Err(Error::Invalid("the download is already being moved".into()));
            }
            moves
        };
        let result = async {
            tokio::task::spawn_blocking(move || move_all(&moves))
                .await
                .map_err(Error::internal)??;
            self.inner.update(id, |r| {
                r.dir = dir;
                r.file_name = name;
            })
        }
        .await;
        self.inner.held.lock().remove(&id);
        self.inner.emit(Event::Updated(result?));
        Ok(())
    }

    /// Stops and forgets a download. With `delete_files`, also deletes its partial data and, if
    /// it completed, the downloaded file.
    pub async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<()> {
        self.pause(id);
        let record = self.wait(id).await?;
        self.inner.store.delete(id)?;
        if delete_files {
            for path in media::working_files(&record) {
                remove_if_exists(&path)?;
            }
            remove_if_exists(&record.part_path())?;
            if record.status == Status::Completed {
                remove_if_exists(&record.path())?;
            }
        }
        self.inner.emit(Event::Removed { id });
        Ok(())
    }

    /// Moves a download to another queue. A running download keeps running.
    pub fn set_download_queue(&self, id: DownloadId, queue: QueueId) -> Result<()> {
        self.queue(queue)?;
        let record = {
            let _sched = self.inner.sched.lock();
            let record = self.inner.update(id, |r| r.queue = queue)?;
            if let Some(active) = self.inner.active.lock().get_mut(&id) {
                active.queue = queue;
            }
            record
        };
        self.inner.emit(Event::Updated(record));
        self.inner.dispatch();
        Ok(())
    }

    /// Limits one download, in bytes per second, on top of the global limit. `None` removes the
    /// limit.
    pub fn set_download_speed_limit(&self, id: DownloadId, limit: Option<u64>) -> Result<()> {
        let limit = limit::normalize(limit);
        let record = self.inner.update(id, |r| r.speed_limit = limit)?;
        if let Some(active) = self.inner.active.lock().get(&id) {
            active.limiter.set(limit);
        }
        self.inner.emit(Event::Updated(record));
        Ok(())
    }

    /// Sets the SHA-256 the file should have, from any text holding one, or forgets it with
    /// `None`. A finished file is checked straight away.
    pub fn set_sha256(&self, id: DownloadId, text: Option<&str>) -> Result<()> {
        let expected = text.map(parse_checksum).transpose()?;
        let record = self.inner.update(id, |r| {
            if r.checksum.as_ref().map(|c| &c.expected) != expected.as_ref() {
                r.checksum =
                    expected.map(|expected| Checksum { expected, state: CheckState::Waiting });
            }
        })?;
        self.inner.emit(Event::ChecksumChanged { id, checksum: record.checksum.clone() });
        if record.checksum.is_some_and(|c| c.state == CheckState::Waiting) {
            self.inner.start_check(id)?;
        }
        Ok(())
    }

    /// Checks the finished file against its SHA-256 again, as after it was put back in place.
    pub fn check_sha256(&self, id: DownloadId) -> Result<()> {
        let record = self.get(id)?;
        if record.checksum.is_none() {
            return Err(Error::Invalid("there is no SHA-256 to check the file against".into()));
        }
        if record.status != Status::Completed {
            return Err(Error::Invalid("the file is checked once the download finishes".into()));
        }
        self.inner.start_check(id)
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(self.inner.settings.lock().clone())
    }

    /// The configuration the engine was opened with, which settings left unset fall back to.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Saves and applies `settings`. Values out of range are brought into it.
    pub fn set_settings(&self, settings: Settings) -> Result<()> {
        self.update_settings(|s| *s = settings)
    }

    /// Limits all downloads together, in bytes per second. `None` removes the limit.
    pub fn set_speed_limit(&self, limit: Option<u64>) -> Result<()> {
        self.update_settings(|s| s.speed_limit = limit)
    }

    fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut current = self.inner.settings.lock();
        let mut settings = current.clone();
        change(&mut settings);
        settings.speed_limit = limit::normalize(settings.speed_limit);
        settings.connections = settings.connections.map(|n| n.clamp(1, MAX_CONNECTIONS));
        settings.auto_retries = settings.auto_retries.map(|n| n.min(MAX_AUTO_RETRIES));
        self.inner.store.save_settings(&settings)?;
        self.inner.speed_limit.set(settings.speed_limit);
        *current = settings.clone();
        // Sent while still holding the lock, so listeners see changes in the order they were made.
        self.inner.emit(Event::SettingsChanged(settings));
        Ok(())
    }

    /// All queues, the main queue first.
    pub fn queues(&self) -> Result<Vec<Queue>> {
        self.inner.store.queues()
    }

    pub fn queue(&self, id: QueueId) -> Result<Queue> {
        self.queues()?.into_iter().find(|q| q.id == id).ok_or(Error::QueueNotFound(id))
    }

    /// Adds a queue under a new ID, ignoring the one in `queue`.
    pub fn add_queue(&self, mut queue: Queue) -> Result<Queue> {
        {
            let _sched = self.inner.sched.lock();
            let queues = self.queues()?;
            queue.id = QueueId(queues.iter().map(|q| q.id.0 + 1).max().unwrap_or(1));
            validate(&mut queue, &queues)?;
            self.inner.store.save_queue(&queue)?;
        }
        self.inner.emit(Event::QueuesChanged);
        Ok(queue)
    }

    /// Saves a changed queue. A higher `max_active` starts waiting downloads right away; after a
    /// lower one, running downloads finish before more start.
    pub fn update_queue(&self, mut queue: Queue) -> Result<()> {
        {
            let _sched = self.inner.sched.lock();
            let queues = self.queues()?;
            if !queues.iter().any(|q| q.id == queue.id) {
                return Err(Error::QueueNotFound(queue.id));
            }
            validate(&mut queue, &queues)?;
            self.inner.store.save_queue(&queue)?;
        }
        self.inner.emit(Event::QueuesChanged);
        self.inner.dispatch();
        Ok(())
    }

    /// Removes a queue and moves its downloads to the main queue.
    pub fn remove_queue(&self, id: QueueId) -> Result<()> {
        if id == QueueId::MAIN {
            return Err(Error::Invalid("the main queue cannot be removed".into()));
        }
        self.queue(id)?;
        let mut moved = Vec::new();
        {
            let _sched = self.inner.sched.lock();
            for record in self.list()?.into_iter().filter(|r| r.queue == id) {
                moved.push(self.inner.update(record.id, |r| r.queue = QueueId::MAIN)?);
            }
            for active in self.inner.active.lock().values_mut().filter(|a| a.queue == id) {
                active.queue = QueueId::MAIN;
            }
            self.inner.store.delete_queue(id)?;
        }
        for record in moved {
            self.inner.emit(Event::Updated(record));
        }
        self.inner.emit(Event::QueuesChanged);
        self.inner.dispatch();
        Ok(())
    }

    /// Starts and stops queues on their schedules until [`Engine::shutdown`]. Only a long-running
    /// process such as the desktop app should run it.
    pub async fn run_scheduler(self) {
        queue::run_scheduler(self).await;
    }

    pub(crate) async fn closed(&self) {
        self.inner.closed.cancelled().await;
    }

    /// Pauses every running download and waits until all of them have saved their progress.
    /// Afterwards the engine starts no more downloads.
    pub async fn shutdown(&self) {
        {
            let _sched = self.inner.sched.lock();
            self.inner.closed.cancel();
        }
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
    }
}

/// A program found for media downloads, with its version, or `None` when it won't run.
async fn tool(found: Option<(PathBuf, bool)>) -> Option<Tool> {
    let (path, managed) = found?;
    match tools::version(&path).await {
        Ok(version) => Some(Tool { version: short_version(&version), path, managed }),
        Err(e) => {
            tracing::warn!("checking {}: {e}", path.display());
            None
        }
    }
}

/// `2.9.8` from deno's `deno 2.9.8 (stable, release, …)`. yt-dlp prints the version alone.
fn short_version(text: &str) -> String {
    match text.strip_prefix("deno ") {
        Some(rest) => rest.split_whitespace().next().unwrap_or(rest).to_string(),
        None => text.to_string(),
    }
}

fn parse_checksum(text: &str) -> Result<String> {
    checksum::parse_sha256(text)
        .ok_or_else(|| Error::Invalid("a SHA-256 is 64 characters of 0-9 and a-f".into()))
}

fn parse_url(url: &str) -> Result<url::Url> {
    let url = url::Url::parse(url.trim()).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::InvalidUrl(format!("unsupported scheme `{}`", url.scheme())));
    }
    Ok(url)
}

/// Whether the download finished but the engine stopped before recording it: the `.part` file
/// is gone, and a file of the full size has its name. `finalize` saves the finished progress
/// before renaming, which makes this check possible.
fn finished_on_disk(record: &DownloadRecord) -> bool {
    let Some(total) = record.total else { return false };
    // Running yt-dlp again finds its finished file, and the file's name with it.
    record.media.is_none()
        && !record.segments.is_empty()
        && record.downloaded() == total
        && !record.part_path().exists()
        && std::fs::metadata(record.path()).is_ok_and(|m| m.is_file() && m.len() == total)
}

/// The files to move so that the download `from` becomes `to`, as `(from, to)` pairs.
fn planned_moves(from: &DownloadRecord, to: &DownloadRecord) -> Result<Vec<(PathBuf, PathBuf)>> {
    if from.path() == to.path() {
        return Ok(Vec::new());
    }
    let moves = if from.status == Status::Completed {
        if !from.path().is_file() {
            return Err(Error::Invalid(format!(
                "the file is no longer at {}",
                from.path().display()
            )));
        }
        vec![(from.path(), to.path())]
    } else if from.media.is_some() {
        let (old, new) = (media::stem(&from.file_name), media::stem(&to.file_name));
        // Only the guessed extension changes, which yt-dlp decides anyway.
        if old == new && from.dir == to.dir {
            return Ok(Vec::new());
        }
        if media::stem_taken(&to.path()) {
            return Err(taken(&to.file_name));
        }
        media::working_files(from)
            .into_iter()
            .filter_map(|path| {
                let name = path.file_name()?.to_string_lossy().into_owned();
                let rest = name.strip_prefix(old)?;
                Some((path.clone(), to.dir.join(format!("{new}{rest}"))))
            })
            .collect()
    } else {
        if to.path().exists() && !same_file_name(&from.path(), &to.path()) {
            return Err(taken(&to.file_name));
        }
        let part = from.part_path();
        if part.exists() { vec![(part, to.part_path())] } else { Vec::new() }
    };
    for (source, target) in &moves {
        if target.exists() && !same_file_name(source, target) {
            return Err(taken(&target.file_name().unwrap_or_default().to_string_lossy()));
        }
    }
    Ok(moves)
}

/// Paths that differ only in case, which some file systems treat as the same file.
fn same_file_name(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

fn taken(name: &str) -> Error {
    Error::Invalid(format!("“{name}” already exists in that folder"))
}

/// Moves the files, or none of them: if one fails, those already moved go back.
fn move_all(moves: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (i, (source, target)) in moves.iter().enumerate() {
        let moved = target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| fsio::move_new(source, target));
        if let Err(e) = moved {
            for (source, target) in moves[..i].iter().rev() {
                if let Err(e) = fsio::move_new(target, source) {
                    tracing::error!("could not move {} back: {e}", target.display());
                }
            }
            return Err(e.into());
        }
    }
    Ok(())
}

/// Checks a queue before it is saved, and trims its name.
fn validate(queue: &mut Queue, queues: &[Queue]) -> Result<()> {
    queue.name = queue.name.trim().to_string();
    if queue.name.is_empty() {
        return Err(Error::Invalid("the queue needs a name".into()));
    }
    if queues.iter().any(|q| q.id != queue.id && q.name.eq_ignore_ascii_case(&queue.name)) {
        return Err(Error::Invalid(format!("a queue named “{}” already exists", queue.name)));
    }
    if queue.max_active == 0 {
        return Err(Error::Invalid("a queue must run at least one download at a time".into()));
    }
    if !queue.schedule.is_valid() {
        return Err(Error::Invalid("the schedule has an invalid time or day".into()));
    }
    Ok(())
}

fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}
