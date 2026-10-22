//! End-to-end tests against a local HTTP server with controllable behavior.

use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::get,
};
use blazer_core::{
    Config, DownloadId, Engine, Event, MAX_CONNECTIONS, NewDownload, Queue, QueueId, RedbStore,
    Settings, Status, Store,
};
use bytes::Bytes;
use tokio::sync::broadcast::error::RecvError;

const CHUNK: usize = 16 * 1024;

struct Server {
    data: Vec<u8>,
    ranges: bool,
    known_length: bool,
    chunk_delay: Duration,
    etag: parking_lot::Mutex<String>,
    /// The next this-many responses abort after `fail_after` bytes.
    failing_responses: AtomicUsize,
    fail_after: usize,
    bytes_sent: AtomicU64,
    range_requests: AtomicUsize,
    /// Responds 429 beyond this many concurrent transfers; 0 means unlimited.
    max_concurrent: usize,
    concurrent: Arc<AtomicUsize>,
    throttled: AtomicUsize,
    /// The next this-many data requests get HTTP 500. The `bytes=0-0` probe is spared.
    error_responses: AtomicUsize,
    /// Breaks the `Content-Range` of partial responses to data requests.
    range_fault: parking_lot::Mutex<Option<RangeFault>>,
    /// Like a CDN seen in the wild: byte ranges give this smaller total, and each one runs on to
    /// the real end of the file, past the bytes it says it holds.
    claimed_len: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum RangeFault {
    Missing,
    WrongStart,
    WrongTotal,
}

/// Counts a response body as in flight until it is dropped.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Server {
    fn new(size: usize) -> Self {
        Self {
            data: test_data(size),
            ranges: true,
            known_length: true,
            chunk_delay: Duration::ZERO,
            etag: parking_lot::Mutex::new("\"v1\"".into()),
            failing_responses: AtomicUsize::new(0),
            fail_after: 0,
            bytes_sent: AtomicU64::new(0),
            range_requests: AtomicUsize::new(0),
            max_concurrent: 0,
            concurrent: Arc::default(),
            throttled: AtomicUsize::new(0),
            error_responses: AtomicUsize::new(0),
            range_fault: parking_lot::Mutex::new(None),
            claimed_len: None,
        }
    }

    /// Starts serving at `/file.bin`, and the same at `/sample`, and returns the first URL.
    async fn spawn(self) -> (Arc<Self>, String) {
        let server = Arc::new(self);
        let app = Router::new()
            .route("/file.bin", get(serve))
            .route("/sample", get(serve))
            .with_state(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (server, url)
    }
}

fn test_data(size: usize) -> Vec<u8> {
    let mut x: u32 = 0x9E37_79B9;
    (0..size)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()))
}

async fn serve(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    let etag = server.etag.lock().clone();
    let len = server.data.len() as u64;
    let if_range_ok = headers.get(header::IF_RANGE).is_none_or(|v| v == etag.as_str());
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range)
        .filter(|_| server.ranges && if_range_ok);
    let probe = headers.get(header::RANGE).is_some_and(|v| v == "bytes=0-0");
    if !probe
        && server
            .error_responses
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::empty())
            .unwrap();
    }

    let in_flight = server.concurrent.fetch_add(1, Ordering::Relaxed) + 1;
    let guard = InFlight(server.concurrent.clone());
    if server.max_concurrent > 0 && in_flight > server.max_concurrent {
        server.throttled.fetch_add(1, Ordering::Relaxed);
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(header::RETRY_AFTER, "0")
            .body(Body::empty())
            .unwrap();
    }

    let mut resp = Response::builder().header(header::ETAG, &etag);
    if server.ranges {
        resp = resp.header(header::ACCEPT_RANGES, "bytes");
    }
    let (start, end) = match range {
        Some((start, _)) if start >= len => {
            return resp
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
        Some((start, end)) => {
            server.range_requests.fetch_add(1, Ordering::Relaxed);
            let end = end.map_or(len, |e| (e + 1).min(len));
            resp = resp.status(StatusCode::PARTIAL_CONTENT);
            let fault = if probe { None } else { *server.range_fault.lock() };
            let content_range = match fault {
                None if server.claimed_len.is_some() => {
                    let claimed = server.claimed_len.unwrap();
                    Some(format!("bytes {start}-{}/{claimed}", end.min(claimed) - 1))
                }
                None => Some(format!("bytes {start}-{}/{len}", end - 1)),
                Some(RangeFault::Missing) => None,
                Some(RangeFault::WrongStart) => Some(format!("bytes {}-{}/{len}", start + 1, end)),
                Some(RangeFault::WrongTotal) => {
                    Some(format!("bytes {start}-{}/{}", end - 1, len + 1))
                }
            };
            if let Some(content_range) = content_range {
                resp = resp.header(header::CONTENT_RANGE, content_range);
            }
            (start, if server.claimed_len.is_some() { len } else { end })
        }
        None => (0, len),
    };
    if server.claimed_len.is_none() && (server.known_length || range.is_some()) {
        resp = resp.header(header::CONTENT_LENGTH, end - start);
    }

    let fail_at = server
        .failing_responses
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .ok()
        .map(|_| server.fail_after);
    let body = futures_util::stream::unfold((start, 0), move |(pos, sent)| {
        let server = server.clone();
        let _guard = &guard;
        async move {
            if pos >= end {
                return None;
            }
            if fail_at.is_some_and(|limit| sent >= limit) {
                return Some((Err(io::Error::other("injected failure")), (end, sent)));
            }
            if !server.chunk_delay.is_zero() {
                tokio::time::sleep(server.chunk_delay).await;
            }
            let n = CHUNK.min((end - pos) as usize);
            let chunk = Bytes::copy_from_slice(&server.data[pos as usize..pos as usize + n]);
            server.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
            Some((Ok(chunk), (pos + n as u64, sent + n)))
        }
    });
    resp.body(Body::from_stream(body)).unwrap()
}

fn config() -> Config {
    Config {
        connections: 4,
        min_split_size: 64 * 1024,
        retry_delay: Duration::from_millis(20),
        progress_interval: Duration::from_millis(20),
        checkpoint_interval: Duration::from_millis(50),
        write_buffer: 64 * 1024,
        ..Config::default()
    }
}

fn open(dir: &Path) -> Engine {
    Engine::open(dir.join("state.redb"), config()).unwrap()
}

/// Gives up on a connection after one retry, and on the download after `auto_retries` quick
/// automatic retries.
fn open_retrying(dir: &Path, auto_retries: u32) -> Engine {
    let config = Config {
        connections: 1,
        max_retries: 1,
        auto_retries,
        auto_retry_delay: Duration::from_millis(30),
        max_auto_retry_delay: Duration::from_millis(100),
        ..config()
    };
    Engine::open(dir.join("state.redb"), config).unwrap()
}

/// Waits until the download has completed, failed or been paused: unlike [`Engine::wait`], not
/// while it waits to retry.
async fn settle(engine: &Engine, id: DownloadId) -> blazer_core::DownloadRecord {
    wait_all(engine, &[id]).await;
    engine.get(id).unwrap()
}

/// Pauses the download once at least `bytes` are downloaded.
async fn pause_after(engine: &Engine, id: DownloadId, bytes: u64) {
    let mut events = engine.subscribe();
    loop {
        match events.recv().await {
            Ok(Event::Progress(p)) if p.id == id && p.downloaded >= bytes => break,
            Ok(Event::StatusChanged { id: e, status, .. })
                if e == id && status != Status::Downloading =>
            {
                panic!("download stopped before it could be paused: {status:?}")
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => unreachable!(),
        }
    }
    engine.pause(id);
}

fn assert_saved(dir: &Path, expected: &[u8]) {
    let saved = std::fs::read(dir.join("file.bin")).unwrap();
    assert!(saved == expected, "downloaded file differs from the source");
    assert!(!dir.join("file.bin.part").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn segmented_download_matches_source() {
    let (server, url) = Server::new(3 * 1024 * 1024 + 123).spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
    // One probe plus at least two segment requests.
    assert!(server.range_requests.load(Ordering::Relaxed) >= 3);
    assert!(record.segments.len() > 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn falls_back_to_one_connection_without_range_support() {
    let mut server = Server::new(1024 * 1024);
    server.ranges = false;
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert!(!record.resumable);
    assert_eq!(server.range_requests.load(Ordering::Relaxed), 0);
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_a_stream_of_unknown_length() {
    let mut server = Server::new(700 * 1024 + 5);
    server.ranges = false;
    server.known_length = false;
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    assert_eq!(engine.get(id).unwrap().total, None);
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_eq!(record.total, Some(server.data.len() as u64));
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_after_restart_without_refetching() {
    let size = 4 * 1024 * 1024;
    let mut server = Server::new(size);
    server.chunk_delay = Duration::from_millis(10);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();

    let engine = open(tmp.path());
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    pause_after(&engine, id, size as u64 / 3).await;
    let paused = engine.wait(id).await.unwrap();
    assert_eq!(paused.status, Status::Paused, "{:?}", paused.error);
    engine.shutdown().await;
    drop(engine);

    // A new engine on the same database, as after an app restart.
    let engine = open(tmp.path());
    let record = engine.get(id).unwrap();
    assert_eq!(record.status, Status::Paused);
    assert!(record.downloaded() >= size as u64 / 3);
    engine.start(id).unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
    let sent = server.bytes_sent.load(Ordering::Relaxed);
    assert!(sent < size as u64 * 5 / 4, "server sent {sent} bytes for a {size}-byte file");
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_dropped_connections() {
    let mut server = Server::new(2 * 1024 * 1024);
    server.failing_responses = AtomicUsize::new(5);
    server.fail_after = 100 * 1024;
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_eq!(server.failing_responses.load(Ordering::Relaxed), 0);
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn detects_a_changed_remote_file_on_resume() {
    let size = 2 * 1024 * 1024;
    let mut server = Server::new(size);
    server.chunk_delay = Duration::from_millis(10);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    pause_after(&engine, id, size as u64 / 4).await;
    engine.wait(id).await.unwrap();

    *server.etag.lock() = "\"v2\"".into();
    engine.start(id).unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.status, Status::Failed);
    assert!(record.error.unwrap().contains("changed"));

    // Restarting fetches the new version from scratch.
    engine.restart(id).await.unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn picks_a_unique_name_and_removes_files() {
    let (_server, url) = Server::new(10_000).spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.bin"), b"existing").unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.file_name, "file (1).bin");
    assert!(record.path().exists());

    engine.remove(id, true).await.unwrap();
    assert!(!record.path().exists());
    assert!(engine.list().unwrap().is_empty());
    assert_eq!(std::fs::read(tmp.path().join("file.bin")).unwrap(), b"existing");
}

#[tokio::test(flavor = "multi_thread")]
async fn sheds_connections_when_the_server_throttles() {
    let mut server = Server::new(2 * 1024 * 1024);
    server.max_concurrent = 2;
    server.chunk_delay = Duration::from_millis(2);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let mut new = NewDownload::new(&url, tmp.path());
    new.connections = Some(8);
    let id = engine.add(new).await.unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
    // Each extra connection is refused about once, then gives up instead of retrying forever.
    let throttled = server.throttled.load(Ordering::Relaxed);
    assert!(throttled <= 12, "server throttled {throttled} requests");
}

/// Waits until none of the downloads is queued, running or waiting to retry.
async fn wait_all(engine: &Engine, ids: &[DownloadId]) {
    let mut events = engine.subscribe();
    let settled = || {
        ids.iter().all(|&id| {
            let status = engine.get(id).unwrap().status;
            !engine.is_active(id) && !matches!(status, Status::Queued | Status::Retrying)
        })
    };
    while !settled() {
        if let Err(RecvError::Closed) = events.recv().await {
            break;
        }
    }
}

fn set_max_active(engine: &Engine, max_active: usize) -> Queue {
    let queue = Queue { max_active, ..engine.queue(QueueId::MAIN).unwrap() };
    engine.update_queue(queue.clone()).unwrap();
    queue
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queue_runs_at_most_max_active_downloads() {
    let mut server = Server::new(512 * 1024);
    server.chunk_delay = Duration::from_millis(5);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    set_max_active(&engine, 2);

    let peak = Arc::new(AtomicUsize::new(0));
    let mut events = engine.subscribe();
    let watcher = tokio::spawn({
        let peak = peak.clone();
        async move {
            let mut running = std::collections::HashSet::new();
            while let Ok(event) = events.recv().await {
                if let Event::StatusChanged { id, status, .. } = event {
                    match status {
                        Status::Downloading => running.insert(id),
                        _ => running.remove(&id),
                    };
                    peak.fetch_max(running.len(), Ordering::Relaxed);
                }
            }
        }
    });

    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(engine.add(NewDownload::new(&url, tmp.path())).await.unwrap());
    }
    assert_eq!(engine.get(ids[4]).unwrap().status, Status::Queued);
    wait_all(&engine, &ids).await;
    engine.shutdown().await;
    drop(engine);
    watcher.await.unwrap();

    assert_eq!(peak.load(Ordering::Relaxed), 2);
    for i in 0..5 {
        let name = if i == 0 { "file.bin".into() } else { format!("file ({i}).bin") };
        assert!(tmp.path().join(name).exists());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_downloads_leave_the_queue() {
    let mut server = Server::new(512 * 1024);
    server.chunk_delay = Duration::from_millis(5);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    set_max_active(&engine, 1);

    let first = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let second = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    assert_eq!(engine.get(second).unwrap().status, Status::Queued);
    engine.pause(second);
    assert_eq!(engine.get(second).unwrap().status, Status::Paused);

    assert_eq!(engine.wait(first).await.unwrap().status, Status::Completed);
    assert_eq!(engine.get(second).unwrap().status, Status::Paused);
    assert!(!engine.is_active(second));

    engine.start_queue(QueueId::MAIN).unwrap();
    assert_eq!(engine.wait(second).await.unwrap().status, Status::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_a_download_to_another_queue_starts_it() {
    let mut server = Server::new(512 * 1024);
    server.chunk_delay = Duration::from_millis(5);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    let main = set_max_active(&engine, 1);
    let other = engine.add_queue(Queue { name: "Other".into(), ..main }).unwrap();
    assert_ne!(other.id, QueueId::MAIN);

    let first = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let second = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    assert!(!engine.is_active(second));
    engine.set_download_queue(second, other.id).unwrap();
    assert!(engine.is_active(second));

    wait_all(&engine, &[first, second]).await;
    engine.remove_queue(other.id).unwrap();
    assert_eq!(engine.get(second).unwrap().queue, QueueId::MAIN);
    assert_eq!(engine.queues().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn speed_limits_slow_downloads_down() {
    let size = 600 * 1024;
    let (server, url) = Server::new(size).spawn().await;
    let tmp = tempfile::tempdir().unwrap();

    let engine = open(tmp.path());
    engine.set_speed_limit(Some(400 * 1024)).unwrap();
    let started = std::time::Instant::now();
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed >= 1.2, "took {elapsed}s at a limit of 1.5s for the whole file");
    engine.shutdown().await;
    drop(engine);

    // The global limit is saved; a lower per-download limit applies on top of it.
    let engine = open(tmp.path());
    assert_eq!(engine.settings().unwrap().speed_limit, Some(400 * 1024));
    let mut new = NewDownload::new(&url, tmp.path());
    new.speed_limit = Some(200 * 1024);
    let started = std::time::Instant::now();
    let id = engine.add(new).await.unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed >= 2.5, "took {elapsed}s at a limit of 3s for the whole file");
}

/// Raw engine throughput on localhost. Run with:
/// `cargo test --release -p blazer-core --test engine -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn throughput() {
    let size = 512 * 1024 * 1024;
    let (server, url) = Server::new(size).spawn().await;
    for connections in [1, 4, 8] {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            tmp.path().join("state.redb"),
            Config { connections, ..Config::default() },
        )
        .unwrap();
        let start = std::time::Instant::now();
        let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
        let record = engine.wait(id).await.unwrap();
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(record.status, Status::Completed);
        assert_saved(tmp.path(), &server.data);
        println!("{connections} connections: {:.0} MiB/s", size as f64 / secs / (1024.0 * 1024.0));
    }
}

async fn ranged_download_with(fault: RangeFault) -> blazer_core::DownloadRecord {
    let server = Server::new(1024 * 1024);
    *server.range_fault.lock() = Some(fault);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = settle(&engine, id).await;
    assert!(!tmp.path().join("file.bin").exists(), "a broken response produced a file");
    record
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_partial_responses_that_do_not_say_where_they_belong() {
    let record = ranged_download_with(RangeFault::Missing).await;
    assert_eq!(record.status, Status::Failed);
    assert!(record.error.unwrap().contains("without saying which part"));

    let record = ranged_download_with(RangeFault::WrongStart).await;
    assert_eq!(record.status, Status::Failed);
    assert!(record.error.unwrap().contains("ignored the byte-range"));
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_in_one_piece_when_ranges_hold_more_than_they_say() {
    let mut server = Server::new(1024 * 1024);
    server.claimed_len = Some(1024 * 1024 - 9000);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = settle(&engine, id).await;

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_eq!(record.total, Some(server.data.len() as u64));
    assert!(!record.resumable, "the server's ranges cannot be trusted");
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn adds_the_extension_the_first_bytes_show() {
    let mut server = Server::new(200 * 1024);
    server.data[..4].copy_from_slice(b"FLV");
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(url.replace("file.bin", "sample"), tmp.path()));
    let record = settle(&engine, id.await.unwrap()).await;

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_eq!(record.file_name, "sample.flv");
    assert_eq!(std::fs::read(tmp.path().join("sample.flv")).unwrap(), server.data);
    assert!(!tmp.path().join("sample").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_range_from_a_file_of_another_size_counts_as_a_change() {
    let record = ranged_download_with(RangeFault::WrongTotal).await;
    assert_eq!(record.status, Status::Failed);
    assert!(record.error.unwrap().contains("changed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_the_whole_download_after_temporary_failures() {
    let server = Server::new(256 * 1024);
    // More than one connection's retries can absorb, so the download as a whole must retry.
    server.error_responses.store(4, Ordering::Relaxed);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open_retrying(tmp.path(), 5);

    let mut events = engine.subscribe();
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = settle(&engine, id).await;

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_eq!(record.retry, None);
    assert_saved(tmp.path(), &server.data);
    let mut retried = 0;
    while let Ok(event) = events.try_recv() {
        if let Event::StatusChanged { status: Status::Retrying, error, .. } = event {
            assert!(error.unwrap().contains("HTTP 500"));
            retried += 1;
        }
    }
    assert!(retried >= 1, "the download never waited to retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn progress_tells_why_connections_are_failing() {
    let server = Server::new(64 * 1024);
    server.error_responses.store(usize::MAX, Ordering::Relaxed);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    // Connections keep retrying on their own, so the download never turns Retrying.
    let config = Config {
        connections: 1,
        max_retries: 1000,
        stall_after: Duration::from_millis(100),
        ..config()
    };
    let engine = Engine::open(tmp.path().join("state.redb"), config).unwrap();

    let mut events = engine.subscribe();
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let problem = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.unwrap() {
                Event::Progress(p) if p.id == id && p.problem.is_some() => {
                    assert!(p.stalled);
                    break p.problem.unwrap();
                }
                Event::StatusChanged { status, .. } => assert_eq!(status, Status::Downloading),
                _ => {}
            }
        }
    })
    .await
    .expect("progress never reported the failing connection");
    assert!(problem.contains("server had a problem"), "{problem}");

    server.error_responses.store(0, Ordering::Relaxed);
    let record = settle(&engine, id).await;
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn inspects_a_link_without_adding_it() {
    let server = Server::new(300 * 1024);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let info = engine.inspect(&url, &[]).await.unwrap();
    assert_eq!(info.total, Some(300 * 1024));
    assert!(info.resumable);
    assert!(engine.list().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn fails_once_the_automatic_retries_run_out() {
    let server = Server::new(64 * 1024);
    server.error_responses.store(usize::MAX, Ordering::Relaxed);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open_retrying(tmp.path(), 2);

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = settle(&engine, id).await;

    assert_eq!(record.status, Status::Failed);
    assert!(record.error.unwrap().contains("server had a problem"));
    assert_eq!(record.retry, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_settings_override_the_config() {
    let server = Server::new(64 * 1024);
    server.error_responses.store(usize::MAX, Ordering::Relaxed);
    let (_server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open_retrying(tmp.path(), 1000);
    let settings =
        Settings { auto_retries: Some(0), connections: Some(1000), ..Settings::default() };
    engine.set_settings(settings).unwrap();

    // Fails at once rather than retrying a thousand times.
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = settle(&engine, id).await;
    assert_eq!(record.status, Status::Failed);
    engine.shutdown().await;
    drop(engine);

    let engine = open_retrying(tmp.path(), 1000);
    let saved = engine.settings().unwrap();
    assert_eq!(saved.auto_retries, Some(0));
    assert_eq!(saved.connections, Some(MAX_CONNECTIONS), "brought into range");
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_cancels_a_pending_retry() {
    let server = Server::new(64 * 1024);
    server.error_responses.store(usize::MAX, Ordering::Relaxed);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        connections: 1,
        max_retries: 0,
        auto_retry_delay: Duration::from_millis(300),
        ..config()
    };
    let engine = Engine::open(tmp.path().join("state.redb"), config).unwrap();

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.status, Status::Retrying);
    engine.pause(id);
    server.error_responses.store(0, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(600)).await;

    let record = engine.get(id).unwrap();
    assert_eq!(record.status, Status::Paused);
    assert_eq!(record.retry, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn recognizes_a_download_that_finished_just_before_a_crash() {
    let (server, url) = Server::new(300 * 1024).spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    assert_eq!(engine.wait(id).await.unwrap().status, Status::Completed);
    engine.shutdown().await;
    drop(engine);

    // As if the file had been renamed but the crash came before the status was saved.
    {
        let store = RedbStore::open(tmp.path().join("state.redb")).unwrap();
        let mut record = store.get(id).unwrap().unwrap();
        record.status = Status::Downloading;
        record.completed_at = None;
        store.save(&record).unwrap();
    }

    let engine = open(tmp.path());
    let record = engine.get(id).unwrap();
    assert_eq!(record.status, Status::Completed);
    assert!(!record.interrupted);
    assert_saved(tmp.path(), &server.data);
    assert!(!tmp.path().join("file (1).bin").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_over_when_the_partial_file_was_changed() {
    let size = 2 * 1024 * 1024;
    let mut server = Server::new(size);
    server.chunk_delay = Duration::from_millis(5);
    let (server, url) = server.spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&url, tmp.path())).await.unwrap();
    pause_after(&engine, id, size as u64 / 3).await;
    engine.wait(id).await.unwrap();
    // Cut short by something else, so the recorded progress no longer matches the file.
    let part = std::fs::OpenOptions::new().write(true).open(tmp.path().join("file.bin.part"));
    part.unwrap().set_len(1000).unwrap();

    engine.start(id).unwrap();
    let record = settle(&engine, id).await;
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &server.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_interrupted_downloads_but_not_paused_ones() {
    let size = 2 * 1024 * 1024;
    let mut server = Server::new(size);
    server.chunk_delay = Duration::from_millis(10);
    let (server, url) = server.spawn().await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let paused = engine.add(NewDownload::new(&url, dir_a.path())).await.unwrap();
    pause_after(&engine, paused, 64 * 1024).await;
    engine.wait(paused).await.unwrap();
    let running = engine.add(NewDownload::new(&url, dir_b.path())).await.unwrap();
    pause_after_shutdown(&engine, running).await;
    drop(engine);

    let engine = open(tmp.path());
    assert!(engine.get(running).unwrap().interrupted);
    assert!(!engine.get(paused).unwrap().interrupted);
    engine.resume_interrupted().unwrap();
    let record = settle(&engine, running).await;

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert!(!record.interrupted);
    assert_saved(dir_b.path(), &server.data);
    assert_eq!(engine.get(paused).unwrap().status, Status::Paused);
}

/// Shuts the engine down once the download is receiving data, as quitting the app does.
async fn pause_after_shutdown(engine: &Engine, id: DownloadId) {
    let mut events = engine.subscribe();
    while !matches!(events.recv().await, Ok(Event::Progress(p)) if p.id == id && p.downloaded > 0) {
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_address_for_the_same_file_keeps_the_progress() {
    let size = 2 * 1024 * 1024;
    let mut old = Server::new(size);
    old.chunk_delay = Duration::from_millis(5);
    let (old, old_url) = old.spawn().await;
    // Paced, so a connection does not run far past a split before it notices.
    let mut new = Server::new(size);
    new.chunk_delay = Duration::from_millis(2);
    let (new, new_url) = new.spawn().await;
    let (_other, other_url) = Server::new(size / 2).spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(NewDownload::new(&old_url, tmp.path())).await.unwrap();
    pause_after(&engine, id, size as u64 / 2).await;
    let paused = engine.wait(id).await.unwrap();

    let err = engine.set_url(id, &other_url).await.unwrap_err();
    assert!(err.to_string().contains("different file"), "{err}");
    engine.set_url(id, &new_url).await.unwrap();
    assert_eq!(engine.get(id).unwrap().url, new_url);

    engine.start(id).unwrap();
    let record = settle(&engine, id).await;
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_saved(tmp.path(), &new.data);
    // The new server sent about what was still missing; splits waste a little in flight.
    let sent = new.bytes_sent.load(Ordering::Relaxed);
    let missing = size as u64 - paused.downloaded();
    assert!(sent < missing * 5 / 4, "sent {sent} for {missing} missing bytes");
    assert!(old.bytes_sent.load(Ordering::Relaxed) < size as u64);
}
