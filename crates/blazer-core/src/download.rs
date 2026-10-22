//! Runs one download: connection workers, progress reports and checkpoints.

use std::{
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
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
    engine::{Inner, MAX_CONNECTIONS},
    error::{Error, Result},
    filename, fsio,
    limit::RateLimiter,
    probe,
    segment::{Claim, Plan},
    types::{self, DownloadId, DownloadRecord, Event, Progress, Status},
};

/// Weight of the newest sample in the smoothed speed.
const SPEED_SMOOTHING: f64 = 0.3;
/// Caps the exponential retry backoff at `retry_delay * 2^5`.
const MAX_BACKOFF_SHIFT: u32 = 5;
/// Longest server-requested `Retry-After` delay that is honored.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// State shared by the connection workers of one download.
struct Shared {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    /// Sent as `If-Range`, so a changed file yields a full 200 response instead of
    /// mismatched bytes.
    validator: Option<String>,
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
    match execute(&inner, &mut record, &cancel, limiter).await {
        Ok(true) => {
            record.status = Status::Completed;
            record.completed_at = Some(types::now());
        }
        Ok(false) => record.status = Status::Paused,
        Err(e) => {
            tracing::warn!(%id, "download failed: {e}");
            record.status = Status::Failed;
            record.error = Some(e.to_string());
        }
    }
    if let Err(e) = inner.finish(&mut record) {
        tracing::error!(%id, "failed to save download: {e}");
    }
    inner.emit(Event::StatusChanged { id, status: record.status, error: record.error.clone() });
    // Its slot in the queue is free now.
    inner.dispatch();
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
    let resume = started && record.resumable && part.exists();
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
        ranged,
        plan: Mutex::new(plan),
        file: Arc::new(file),
        limiter,
        inner: inner.clone(),
        stop: cancel.child_token(),
        fatal: Mutex::new(None),
        workers: AtomicUsize::new(worker_count),
        connections: AtomicUsize::new(0),
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
    tokio::task::spawn_blocking(move || file.sync_all()).await.map_err(Error::internal)??;

    if let Some(e) = fatal.into_inner() {
        return Err(e);
    }
    if complete {
        finalize(record)?;
        return Ok(true);
    }
    if cancel.is_cancelled() {
        return Ok(false);
    }
    Err(last_error.unwrap_or(Error::Incomplete))
}

/// Moves the finished `.part` file to its final name, picking a new name if one appeared there
/// in the meantime.
fn finalize(record: &mut DownloadRecord) -> Result<()> {
    let part = record.part_path();
    if record.path().exists() {
        record.file_name = filename::unique(&record.dir, &record.file_name, |p| p.exists());
    }
    std::fs::rename(part, record.path())?;
    Ok(())
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
        let complete = {
            let mut plan = sh.plan.lock();
            plan.release(claim.idx);
            plan.is_complete()
        };
        if complete {
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
                let backoff = sh.retry_delay * (1 << (failures - 1).min(MAX_BACKOFF_SHIFT));
                let delay = match &e {
                    Error::Throttled { retry_after: Some(after), .. } => {
                        (*after).min(MAX_RETRY_AFTER)
                    }
                    _ => backoff,
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
        if let Some(start) = probe::content_range(resp.headers()).and_then(|r| r.start)
            && start != claim.from
        {
            return Err(Error::RangeIgnored);
        }
    } else if !status.is_success() {
        return Err(Error::from_response(status, resp.headers()));
    }

    let _connection = ConnectionGuard::new(&sh.connections);
    let mut stream = resp.bytes_stream();
    let mut offset = claim.from;
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
                let (take, done) = sh.plan.lock().reserve(claim.idx, chunk.len());
                buf.extend_from_slice(&chunk[..take]);
                if take > 0 {
                    *failures = 0;
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

impl Shared {
    /// Waits as long as the speed limits require after receiving `n` bytes.
    async fn throttle(&self, n: usize) {
        if n > 0 {
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
        inner.emit(Event::Progress(Progress {
            id: record.id,
            downloaded,
            total: record.total,
            speed: speed as u64,
            connections: shared.connections.load(Ordering::Relaxed),
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
