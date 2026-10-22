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
use blazer_core::{Config, DownloadId, Engine, Event, NewDownload, Queue, QueueId, Status};
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
        }
    }

    /// Starts serving at `/file.bin` and returns the URL.
    async fn spawn(self) -> (Arc<Self>, String) {
        let server = Arc::new(self);
        let app = Router::new().route("/file.bin", get(serve)).with_state(server.clone());
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
            resp = resp
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{}/{len}", end - 1));
            (start, end)
        }
        None => (0, len),
    };
    if server.known_length || range.is_some() {
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

/// Waits until none of the downloads is queued or running.
async fn wait_all(engine: &Engine, ids: &[DownloadId]) {
    let mut events = engine.subscribe();
    let settled = || {
        ids.iter().all(|&id| {
            let status = engine.get(id).unwrap().status;
            !engine.is_active(id) && status != Status::Queued
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
