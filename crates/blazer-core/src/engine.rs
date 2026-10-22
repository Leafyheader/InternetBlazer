//! The public engine API: adding, starting, pausing and observing downloads, and managing queues
//! and speed limits.

use std::{collections::HashMap, io, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    download,
    error::{Error, Result},
    filename,
    limit::{self, RateLimiter},
    probe,
    queue::{self, Queue},
    store::{RedbStore, Store},
    types::{self, DownloadId, DownloadRecord, Event, QueueId, Settings, Status},
};

pub const MAX_CONNECTIONS: usize = 32;
const EVENT_CAPACITY: usize = 1024;

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
    pub connect_timeout: Duration,
    /// Drops a connection that receives nothing for this long.
    pub read_timeout: Duration,
    /// Bytes each connection buffers before writing to disk.
    pub write_buffer: usize,
    pub progress_interval: Duration,
    /// How often progress is flushed to disk and saved to the store.
    pub checkpoint_interval: Duration,
    pub user_agent: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connections: 8,
            min_split_size: 1024 * 1024,
            max_retries: 8,
            retry_delay: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            write_buffer: 512 * 1024,
            progress_interval: Duration::from_millis(250),
            checkpoint_interval: Duration::from_secs(1),
            user_agent: concat!("InternetBlazer/", env!("CARGO_PKG_VERSION")).to_string(),
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
    /// Queue it to start instead of adding it paused.
    pub start: bool,
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
            start: true,
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
    closed: CancellationToken,
    /// Held while deciding what runs, so that starting, pausing and finishing downloads see each
    /// other's status changes. Taken before `records` and `active`.
    sched: Mutex<()>,
    /// Held for read-modify-write updates of stored records.
    records: Mutex<()>,
    /// The global limit, shared by every connection of every download.
    pub(crate) speed_limit: RateLimiter,
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
        }
        self.store.save(record)
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

    /// Queues or pauses a download that is not running. The caller holds `sched`.
    fn set_status(&self, id: DownloadId, status: Status) -> Result<()> {
        self.update(id, |r| {
            r.status = status;
            r.error = None;
        })?;
        self.emit(Event::StatusChanged { id, status, error: None });
        Ok(())
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
        // A download still marked as running was cut off by a crash or power loss. Queued ones
        // are paused too, so opening the engine (say, to list downloads) never starts any.
        for mut record in store.list()? {
            if matches!(record.status, Status::Downloading | Status::Queued) {
                record.status = Status::Paused;
                store.save(&record)?;
            }
        }
        if !store.queues()?.iter().any(|q| q.id == QueueId::MAIN) {
            store.save_queue(&Queue::main())?;
        }
        let speed_limit = RateLimiter::new(store.settings()?.speed_limit);
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

    /// Probes the URL, picks a unique file name and records the download.
    pub async fn add(&self, new: NewDownload) -> Result<DownloadId> {
        let queue = new.queue.unwrap_or(QueueId::MAIN);
        self.queue(queue)?;
        let url = url::Url::parse(&new.url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl(format!("unsupported scheme `{}`", url.scheme())));
        }
        let info = probe::probe(&self.inner.client, url.as_str(), &new.headers).await?;

        let suggested = new.file_name.as_deref().or(info.file_name.as_deref());
        let name = filename::sanitize(suggested.unwrap_or("download"));
        let in_progress: Vec<PathBuf> = self
            .list()?
            .into_iter()
            .filter(|r| r.status != Status::Completed)
            .map(|r| r.path())
            .collect();
        let file_name = filename::unique(&new.dir, &name, |path| {
            path.exists()
                || filename::part_path(path).exists()
                || in_progress.iter().any(|p| p == path)
        });

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
            segments: Vec::new(),
            status: if new.start { Status::Queued } else { Status::Paused },
            error: None,
            added_at: types::now(),
            completed_at: None,
        };
        self.inner.store.save(&record)?;
        self.inner.emit(Event::Added(record));
        if new.start {
            self.inner.dispatch();
        }
        Ok(id)
    }

    /// Queues a paused or failed download. It starts as soon as its queue has a free slot.
    pub fn start(&self, id: DownloadId) -> Result<()> {
        self.get(id)?;
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
                if matches!(record.status, Status::Paused | Status::Failed)
                    && !self.is_active(record.id)
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
            } else if record.status == Status::Queued
                && let Err(e) = self.inner.set_status(record.id, Status::Paused)
            {
                tracing::error!(id = %record.id, "failed to pause download: {e}");
            }
        }
    }

    /// Waits until the download is not running, and returns its final record.
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
        let info = probe::probe(&self.inner.client, &record.url, &record.headers).await?;
        {
            let _sched = self.inner.sched.lock();
            self.inner.update(id, |r| {
                r.total = info.total;
                r.resumable = info.resumable;
                r.etag = info.etag;
                r.last_modified = info.last_modified;
                r.segments.clear();
                r.status = Status::Queued;
                r.error = None;
                r.completed_at = None;
            })?;
        }
        self.inner.emit(Event::StatusChanged { id, status: Status::Queued, error: None });
        self.inner.dispatch();
        Ok(())
    }

    /// Stops and forgets a download. With `delete_files`, also deletes its partial data and, if
    /// it completed, the downloaded file.
    pub async fn remove(&self, id: DownloadId, delete_files: bool) -> Result<()> {
        self.pause(id);
        let record = self.wait(id).await?;
        self.inner.store.delete(id)?;
        if delete_files {
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

    pub fn settings(&self) -> Result<Settings> {
        self.inner.store.settings()
    }

    /// Limits all downloads together, in bytes per second. `None` removes the limit.
    pub fn set_speed_limit(&self, limit: Option<u64>) -> Result<()> {
        let settings = Settings { speed_limit: limit::normalize(limit) };
        self.inner.store.save_settings(&settings)?;
        self.inner.speed_limit.set(settings.speed_limit);
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
