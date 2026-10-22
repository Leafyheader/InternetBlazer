//! Runs one download: connection workers, progress reports and checkpoints.

use std::{
    fs::File,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::{StatusCode, header};
use tokio::{
    task::JoinSet,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::{
    engine::{Config, Inner, MAX_CONNECTIONS},
    error::{Error, Result},
    filename, fsio,
    limit::RateLimiter,
    probe,
    segment::{Claim, Plan},
    sniff,
    types::{self, DownloadId, DownloadRecord, Event, Progress, Retry, Status},
};

/// Weight of the newest sample in the smoothed speed.
const SPEED_SMOOTHING: f64 = 0.3;
/// Caps the exponential retry backoff at `retry_delay * 2^5`.
const MAX_BACKOFF_SHIFT: u32 = 5;
/// Longest server-requested `Retry-After` delay that is honored.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long a finished range may take to end its response.
const END_CHECK: Duration = Duration::from_secs(2);

/// State shared by the connection workers of one download.
struct Shared {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    /// Sent as `If-Range`, so a changed file yields a full 200 response instead of
    /// mismatched bytes.
    validator: Option<String>,
    /// The size the download expects, which ranged responses must agree with.
    total: Option<u64>,
    ranged: bool,
    plan: Mutex<Plan>,
    file: Arc<File>,
    /// This download's own limit; the global one is in [`Inner`].
    limiter: Arc<RateLimiter>,
    inner: Arc<Inner>,
    stop: CancellationToken,
    /// An error that dooms the whole download, such as the file changing on the server.
    fatal: Mutex<Option<Error>>,
    /// Workers still running.
    workers: AtomicUsize,
    connections: AtomicUsize,
    /// Bytes received so far, counted before they are written, so that slow progress shows
    /// before a write buffer fills.
    received: AtomicU64,
    /// Connections waiting for a speed limit, which is no reason to call the download stalled.
    throttled: AtomicUsize,
    /// Why a connection last failed. The reporter clears it once data arrives again.
    trouble: Mutex<Option<String>>,
    max_retries: u32,
    retry_delay: Duration,
    write_buffer: usize,
}

pub(crate) async fn run(
    inner: Arc<Inner>,
    id: DownloadId,
    cancel: CancellationToken,
    limiter: Arc<RateLimiter>,
) {
    let mut record = match inner.store.get(id) {
        Ok(Some(record)) => record,
        result => {
            if let Err(e) = result {
                tracing::error!(%id, "failed to load download: {e}");
            }
            inner.deactivate(id);
            inner.emit(Event::Removed { id });
            return;
        }
    };
    let before = record.downloaded();
    let previous = record.retry.take();
    let mut retry_in = None;
    let mut result = execute(&inner, &mut record, &cancel, limiter.clone()).await;
    if matches!(result, Err(Error::RangeOverflow)) {
        // Some CDNs give byte ranges the size of a compressed copy while sending the real file.
        // A plain request for the whole file, which such servers get right, is the way out.
        tracing::warn!(%id, "server's byte ranges are unreliable; downloading in one piece");
        record.resumable = false;
        record.total = None;
        record.segments.clear();
        result = execute(&inner, &mut record, &cancel, limiter).await;
    }
    match result {
        Ok(true) => {
            record.status = Status::Completed;
            record.completed_at = Some(types::now());
        }
        Ok(false) => {
            record.status = Status::Paused;
            // Stopped by the engine shutting down, not by someone pausing it.
            record.interrupted = inner.closed.is_cancelled();
        }
        Err(e) => {
            record.error = Some(e.to_string());
            // An attempt that got somewhere starts the count over.
            let attempt = match previous {
                Some(p) if record.downloaded() <= before => p.attempt + 1,
                _ => 1,
            };
            if e.is_retryable() && attempt <= inner.config.auto_retries {
                let delay = retry_delay(&inner.config, attempt, e.retry_after());
                tracing::info!(%id, "retrying in {delay:?} (attempt {attempt}) after: {e}");
                record.status = Status::Retrying;
                let retry = Retry { attempt, at: types::now() + delay.as_secs() };
                record.retry = Some(retry);
                retry_in = Some((retry, delay));
            } else {
                tracing::warn!(%id, "download failed: {e}");
                record.status = Status::Failed;
            }
        }
    }
    if let Err(e) = inner.finish(&mut record) {
        tracing::error!(%id, "failed to save download: {e}");
    }
    inner.emit(Event::StatusChanged { id, status: record.status, error: record.error.clone() });
    if let Some((retry, delay)) = retry_in {
        inner.retry_later(id, retry, delay);
    }
    // Its slot in the queue is free now.
    inner.dispatch();
}

/// Waits longer after each failed attempt, up to a limit, and longer still if the server asks.
fn retry_delay(cfg: &Config, attempt: u32, retry_after: Option<Duration>) -> Duration {
    let backoff = cfg.auto_retry_delay.saturating_mul(1 << (attempt - 1).min(16));
    jitter(backoff.min(cfg.max_auto_retry_delay)).max(retry_after.unwrap_or_default())
}

/// Spreads retries over ±20% of `delay`, so connections and downloads that failed together do
/// not all come back at the same moment.
fn jitter(delay: Duration) -> Duration {
    delay.mul_f64(fastrand::f64().mul_add(0.4, 0.8))
}

/// Returns `Ok(true)` once complete and `Ok(false)` when paused.
async fn execute(
    inner: &Arc<Inner>,
    record: &mut DownloadRecord,
    cancel: &CancellationToken,
    limiter: Arc<RateLimiter>,
) -> Result<bool> {
    let cfg = &inner.config;
    record.status = Status::Downloading;
    record.error = None;
    inner.save_running(record)?;
    inner.emit(Event::StatusChanged { id: record.id, status: record.status, error: None });

    let part = record.part_path();
    let started = record.segments.iter().any(|s| s.downloaded > 0);
    // A fresh download sets the length of the file to the full size up front, so any other
    // length means someone else changed it, and the progress cannot be trusted.
    let part_len = std::fs::metadata(&part).ok().map(|m| m.len());
    let resume = started && record.resumable && part_len.is_some() && part_len == record.total;
    if started && !resume {
        tracing::info!(id = %record.id, "partial data unusable; starting over");
    }
    if let Some(total) = record.total {
        check_space(record, &part, total)?;
    }
    if resume {
        let info = tokio::select! {
            info = probe::probe(&inner.client, &record.url, &record.headers) => info?,
            () = cancel.cancelled() => return Ok(false),
        };
        if info.total != record.total
            || !info.matches(record.etag.as_deref(), record.last_modified.as_deref())
        {
            return Err(Error::RemoteChanged);
        }
    } else {
        record.segments.clear();
    }

    let total = record.total;
    let file = tokio::task::spawn_blocking(move || fsio::open_part(&part, total, resume))
        .await
        .map_err(Error::internal)??;
    let ranged = record.resumable && record.total.is_some();
    let worker_count = match ranged {
        true => record.connections.unwrap_or(cfg.connections).clamp(1, MAX_CONNECTIONS),
        false => 1,
    };
    let plan = if record.segments.is_empty() {
        Plan::new(record.total, ranged, cfg.min_split_size, worker_count)
    } else {
        Plan::from_segments(record.segments.clone(), ranged, cfg.min_split_size)
    };
    let shared = Arc::new(Shared {
        client: inner.client.clone(),
        url: record.url.clone(),
        headers: record.headers.clone(),
        validator: probe::if_range(record.etag.as_deref(), record.last_modified.as_deref()),
        total: record.total,
        ranged,
        plan: Mutex::new(plan),
        file: Arc::new(file),
        limiter,
        inner: inner.clone(),
        stop: cancel.child_token(),
        fatal: Mutex::new(None),
        workers: AtomicUsize::new(worker_count),
        connections: AtomicUsize::new(0),
        received: AtomicU64::new(0),
        throttled: AtomicUsize::new(0),
        trouble: Mutex::new(None),
        max_retries: cfg.max_retries,
        retry_delay: cfg.retry_delay,
        write_buffer: cfg.write_buffer,
    });

    let mut workers = JoinSet::new();
    for _ in 0..worker_count {
        workers.spawn(worker(shared.clone()));
    }
    let reporter_stop = CancellationToken::new();
    let reporter =
        tokio::spawn(report(inner.clone(), shared.clone(), record.clone(), reporter_stop.clone()));

    let mut last_error = None;
    while let Some(joined) = workers.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => last_error = Some(e),
            Err(e) => last_error = Some(Error::internal(e)),
        }
    }
    reporter_stop.cancel();
    let _ = reporter.await;

    let Shared { plan, file, fatal, .. } =
        Arc::into_inner(shared).expect("workers and reporter have finished");
    let plan = plan.into_inner();
    record.segments = plan.segments();
    let complete = plan.is_complete();
    if complete && record.total.is_none() {
        record.total = Some(plan.downloaded());
    }
    // Also closes the file, which Windows requires before the rename below.
    let len = tokio::task::spawn_blocking(move || {
        file.sync_all()?;
        file.metadata().map(|m| m.len())
    })
    .await
    .map_err(Error::internal)??;

    if let Some(e) = fatal.into_inner() {
        return Err(e);
    }
    if complete {
        let expected = record.total.unwrap_or(len);
        if len != expected {
            return Err(Error::WrongSize { expected, actual: len });
        }
        finalize(inner, record)?;
        return Ok(true);
    }
    if cancel.is_cancelled() {
        return Ok(false);
    }
    Err(last_error.unwrap_or(Error::Incomplete))
}

/// Fails early when the rest of the download cannot fit on the disk. The `.part` file is
/// sparse, so the space it will need is not taken up front. Whatever it takes already counts as
/// available: it is either kept or freed by starting over.
fn check_space(record: &DownloadRecord, part: &Path, total: u64) -> Result<()> {
    let needed = total.saturating_sub(fsio::allocated_size(part));
    match fsio::available_space(&record.dir) {
        Ok(available) if available < needed => Err(Error::NoSpace { needed, available }),
        Ok(_) => Ok(()),
        // Some network drives do not say; the write fails clearly enough if it comes to that.
        Err(e) => {
            tracing::debug!(id = %record.id, "could not check free disk space: {e}");
            Ok(())
        }
    }
}

/// Moves the finished `.part` file to its final name, never over another file: if one took the
/// name in the meantime, it picks a new one. The finished progress and the name are saved
/// before the rename, so that after a crash right behind it, opening the engine recognizes the
/// download as complete.
///
/// A name without an extension gets one from the file's first bytes, if they tell the type, so
/// the file opens with the right program.
fn finalize(inner: &Inner, record: &mut DownloadRecord) -> Result<()> {
    let part = record.part_path();
    if !sniff::has_extension(&record.file_name) {
        match sniff::head(&part).map(|head| sniff::extension(&head)) {
            Ok(Some(ext)) => record.file_name = format!("{}.{ext}", record.file_name),
            Ok(None) => {}
            Err(e) => tracing::debug!(id = %record.id, "could not read the file's type: {e}"),
        }
    }
    loop {
        if record.path().exists() {
            record.file_name = filename::unique(&record.dir, &record.file_name, |p| p.exists());
        }
        inner.save_running(record)?;
        match fsio::rename_new(&part, &record.path()) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            result => return Ok(result?),
        }
    }
}

/// One connection: claims segments and downloads them until no work is left.
async fn worker(sh: Arc<Shared>) -> Result<()> {
    let result = worker_loop(&sh).await;
    // A retired worker already deregistered itself.
    if !matches!(result, Ok(Exit::Retired)) {
        sh.workers.fetch_sub(1, Ordering::Relaxed);
    }
    result.map(drop)
}

enum Exit {
    Finished,
    Retired,
}

async fn worker_loop(sh: &Shared) -> Result<Exit> {
    let mut buf = Vec::with_capacity(sh.write_buffer);
    let mut failures = 0;
    loop {
        if sh.stop.is_cancelled() {
            return Ok(Exit::Finished);
        }
        let Some(claim) = sh.plan.lock().claim() else { return Ok(Exit::Finished) };
        let result = fetch(sh, claim, &mut buf, &mut failures).await;
        // Such as a server found out on the last bytes, which must not pass as complete.
        let fatal = matches!(&result, Err(e) if !e.is_retryable());
        let complete = {
            let mut plan = sh.plan.lock();
            plan.release(claim.idx);
            plan.is_complete()
        };
        if complete && !fatal {
            // Wakes connections sleeping before a retry, so the download ends now.
            sh.stop.cancel();
            return Ok(Exit::Finished);
        }
        match result {
            Ok(()) => {}
            // The server wants fewer connections: give this one up while others keep going.
            // The last worker never retires, so one connection always remains.
            Err(Error::Throttled { .. })
                if sh
                    .workers
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n > 1).then(|| n - 1))
                    .is_ok() =>
            {
                tracing::debug!("server is throttling; dropping a connection");
                return Ok(Exit::Retired);
            }
            Err(e) if e.is_retryable() && failures < sh.max_retries => {
                failures += 1;
                *sh.trouble.lock() = Some(e.to_string());
                let backoff = sh.retry_delay * (1 << (failures - 1).min(MAX_BACKOFF_SHIFT));
                let delay = match e.retry_after() {
                    Some(after) => after.min(MAX_RETRY_AFTER),
                    None => jitter(backoff),
                };
                tracing::debug!("retrying in {delay:?} after: {e}");
                tokio::select! {
                    () = time::sleep(delay) => {}
                    () = sh.stop.cancelled() => return Ok(Exit::Finished),
                }
            }
            // Out of retries: other connections may still finish the work.
            Err(e) if e.is_retryable() => return Err(e),
            Err(e) => {
                sh.fatal.lock().get_or_insert(e);
                sh.stop.cancel();
                return Ok(Exit::Finished);
            }
        }
    }
}

/// Downloads one claimed segment until it is done, the connection fails, or the download stops.
async fn fetch(sh: &Shared, claim: Claim, buf: &mut Vec<u8>, failures: &mut u32) -> Result<()> {
    let mut req = probe::request(&sh.client, &sh.url, &sh.headers);
    if sh.ranged {
        req = req.header(header::RANGE, format!("bytes={}-{}", claim.from, claim.end - 1));
        if let Some(validator) = &sh.validator {
            req = req.header(header::IF_RANGE, validator);
        }
    }
    let resp = tokio::select! {
        resp = req.send() => resp?,
        () = sh.stop.cancelled() => return Ok(()),
    };
    let status = resp.status();
    if sh.ranged {
        match status {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK if sh.validator.is_some() => return Err(Error::RemoteChanged),
            StatusCode::OK => return Err(Error::RangeIgnored),
            status => return Err(Error::from_response(status, resp.headers())),
        }
        // Bytes from the wrong place, or from a file of another size, would corrupt the file.
        let Some(range) = probe::content_range(resp.headers()) else {
            return Err(Error::MalformedRange);
        };
        if range.total.is_some() && range.total != sh.total {
            return Err(Error::RemoteChanged);
        }
        if range.start != Some(claim.from) {
            return Err(Error::RangeIgnored);
        }
    } else if !status.is_success() {
        return Err(Error::from_response(status, resp.headers()));
    }

    let _connection = ConnectionGuard::new(&sh.connections);
    let mut stream = resp.bytes_stream();
    let mut offset = claim.from;
    // What this response may hold. The claim can shrink while it runs, when another
    // connection takes over its end, but the request cannot.
    let requested = claim.end - claim.from;
    let mut body = 0u64;
    buf.clear();
    loop {
        let next = tokio::select! {
            biased;
            () = sh.stop.cancelled() => {
                flush(sh, claim.idx, offset, buf).await?;
                return Ok(());
            }
            next = stream.next() => next,
        };
        match next {
            Some(Ok(chunk)) => {
                body += chunk.len() as u64;
                if sh.ranged && body > requested {
                    return Err(Error::RangeOverflow);
                }
                let (take, done) = sh.plan.lock().reserve(claim.idx, chunk.len());
                buf.extend_from_slice(&chunk[..take]);
                if take > 0 {
                    *failures = 0;
                    sh.received.fetch_add(take as u64, Ordering::Relaxed);
                }
                if done || buf.len() >= sh.write_buffer {
                    offset = flush(sh, claim.idx, offset, buf).await?;
                }
                let stopped = tokio::select! {
                    () = sh.throttle(take) => false,
                    () = sh.stop.cancelled() => true,
                };
                if done || stopped {
                    flush(sh, claim.idx, offset, buf).await?;
                    if done && sh.ranged && body == requested {
                        return check_ended(sh, &mut stream).await;
                    }
                    return Ok(());
                }
            }
            Some(Err(e)) => {
                flush(sh, claim.idx, offset, buf).await?;
                return Err(e.into());
            }
            None => {
                flush(sh, claim.idx, offset, buf).await?;
                let mut plan = sh.plan.lock();
                return match plan.is_unbounded(claim.idx) {
                    true => {
                        plan.finish_unbounded(claim.idx);
                        Ok(())
                    }
                    false => Err(Error::Truncated),
                };
            }
        }
    }
}

/// After all the bytes a range asked for, the response must end. A server that sends more
/// gives byte ranges of a different file than it serves.
async fn check_ended<S, T, E>(sh: &Shared, stream: &mut S) -> Result<()>
where
    S: futures_util::Stream<Item = std::result::Result<T, E>> + Unpin,
    T: AsRef<[u8]>,
{
    let next = tokio::select! {
        next = time::timeout(END_CHECK, stream.next()) => next,
        () = sh.stop.cancelled() => return Ok(()),
    };
    match next {
        Ok(Some(Ok(chunk))) if !chunk.as_ref().is_empty() => Err(Error::RangeOverflow),
        // The end, a connection problem after all the data, or a server that keeps the
        // connection open: the data itself is complete either way.
        _ => Ok(()),
    }
}

impl Shared {
    /// Waits as long as the speed limits require after receiving `n` bytes.
    async fn throttle(&self, n: usize) {
        if n > 0 {
            let _throttled = ConnectionGuard::new(&self.throttled);
            self.inner.speed_limit.take(n).await;
            self.limiter.take(n).await;
        }
    }
}

/// Writes the buffered bytes at `offset` and marks them downloaded. Returns the next offset.
async fn flush(sh: &Shared, idx: usize, offset: u64, buf: &mut Vec<u8>) -> Result<u64> {
    if buf.is_empty() {
        return Ok(offset);
    }
    let data = std::mem::take(buf);
    let file = sh.file.clone();
    let data =
        tokio::task::spawn_blocking(move || fsio::write_at(&file, &data, offset).map(|()| data))
            .await
            .map_err(Error::internal)??;
    let len = data.len() as u64;
    sh.plan.lock().commit(idx, len);
    *buf = data;
    buf.clear();
    Ok(offset + len)
}

/// Emits progress events and periodically checkpoints until `stop` fires.
async fn report(
    inner: Arc<Inner>,
    shared: Arc<Shared>,
    mut record: DownloadRecord,
    stop: CancellationToken,
) {
    let cfg = &inner.config;
    let mut ticker = time::interval(cfg.progress_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_checkpoint = Instant::now();
    let mut last_sample = (Instant::now(), shared.plan.lock().downloaded());
    let mut speed = 0.0;
    let mut received = 0;
    let mut last_data = Instant::now();
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = stop.cancelled() => break,
        }
        let (downloaded, segments) = {
            let plan = shared.plan.lock();
            (plan.downloaded(), plan.segments())
        };
        let now = Instant::now();
        let elapsed = now.duration_since(last_sample.0).as_secs_f64();
        if elapsed > 0.0 {
            let sample = downloaded.saturating_sub(last_sample.1) as f64 / elapsed;
            speed = match speed == 0.0 {
                true => sample,
                false => SPEED_SMOOTHING * sample + (1.0 - SPEED_SMOOTHING) * speed,
            };
        }
        last_sample = (now, downloaded);
        let now_received = shared.received.load(Ordering::Relaxed);
        if now_received != received || shared.throttled.load(Ordering::Relaxed) > 0 {
            received = now_received;
            last_data = now;
            // Data flows again, so whatever went wrong before is over.
            *shared.trouble.lock() = None;
        }
        let stalled = now.duration_since(last_data) >= cfg.stall_after;
        inner.emit(Event::Progress(Progress {
            id: record.id,
            downloaded,
            total: record.total,
            speed: speed as u64,
            connections: shared.connections.load(Ordering::Relaxed),
            stalled,
            problem: if stalled { shared.trouble.lock().clone() } else { None },
            segments: segments.clone(),
        }));

        if now.duration_since(last_checkpoint) >= cfg.checkpoint_interval {
            last_checkpoint = now;
            record.segments = segments;
            checkpoint(&inner, &shared.file, &record).await;
        }
    }
}

/// Flushes written data to disk, then saves it as downloaded. In that order a crash can lose
/// recent progress, but never records bytes that are not on disk.
async fn checkpoint(inner: &Arc<Inner>, file: &Arc<File>, record: &DownloadRecord) {
    let (inner, file, record) = (inner.clone(), file.clone(), record.clone());
    let id = record.id;
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        file.sync_data()?;
        inner.save_running(&mut record.clone())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(%id, "checkpoint failed: {e}"),
        Err(e) => tracing::warn!(%id, "checkpoint task failed: {e}"),
    }
}

/// Counts a connection as active while it is receiving data.
struct ConnectionGuard<'a>(&'a AtomicUsize);

impl<'a> ConnectionGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
