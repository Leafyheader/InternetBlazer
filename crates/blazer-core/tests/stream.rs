//! HLS streams downloaded by the engine itself, against a local server.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use axum::{
    Router,
    body::Body,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use blazer_core::{Config, Engine, Event, MediaFormat, NewDownload, Status};
use parking_lot::Mutex;
use tokio::sync::broadcast::error::RecvError;

const KEY: [u8; 16] = *b"0123456789abcdef";
const IV: [u8; 16] = *b"fedcba9876543210";
/// Pieces 0 to 5 are files of their own, the first three encrypted; 6 to 9 are parts of one file.
const PIECES: usize = 10;
const PIECE: usize = 40 * 1024;

struct Server {
    files: HashMap<String, Vec<u8>>,
    /// What the finished 1080p video must hold.
    expected: Vec<u8>,
    requests: Mutex<HashMap<String, usize>>,
    delay: Duration,
}

fn data(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn encrypt(plain: &[u8], iv: [u8; 16]) -> Vec<u8> {
    let mut buf = plain.to_vec();
    buf.resize(plain.len() + 16, 0);
    let len = cbc::Encryptor::<aes::Aes128>::new(&KEY.into(), &iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf
}

impl Server {
    fn new(delay: Duration) -> Self {
        let mut files = HashMap::new();
        let init = data(1000, 700);
        let mut expected = init.clone();
        let mut media = String::from(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-MAP:URI=\"init.mp4\"\n",
        );
        files.insert("init.mp4".into(), init);
        let mut all = Vec::new();
        for i in 0..PIECES {
            // Sizes that aren't a multiple of the cipher's block.
            let plain = data(i as u32, PIECE + i * 7);
            expected.extend_from_slice(&plain);
            match i {
                0 => media.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/1\"\n"),
                2 => media.push_str(&format!(
                    "#EXT-X-KEY:METHOD=AES-128,URI=\"/keys/1\",IV=0x{}\n",
                    IV.iter().map(|b| format!("{b:02x}")).collect::<String>()
                )),
                3 => media.push_str("#EXT-X-KEY:METHOD=NONE\n"),
                _ => {}
            }
            media.push_str("#EXTINF:4.0,\n");
            let stored = match i {
                // The media sequence number, as the IV.
                0 | 1 => encrypt(&plain, (5 + i as u128).to_be_bytes()),
                2 => encrypt(&plain, IV),
                _ => plain,
            };
            if i < 6 {
                media.push_str(&format!("p{i}.m4s\n"));
                files.insert(format!("p{i}.m4s"), stored);
            } else {
                media.push_str(&format!(
                    "#EXT-X-BYTERANGE:{}@{}\nall.mp4\n",
                    stored.len(),
                    all.len()
                ));
                all.extend_from_slice(&stored);
            }
        }
        media.push_str("#EXT-X-ENDLIST\n");
        files.insert("all.mp4".into(), all);
        files.insert("1080.m3u8".into(), media.into_bytes());
        // Another quality, which must not be what gets downloaded.
        files
            .insert("360.m3u8".into(), b"#EXTM3U\n#EXTINF:4,\nother.ts\n#EXT-X-ENDLIST\n".to_vec());
        files.insert(
            "master.m3u8".into(),
            b"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
360.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080
1080.m3u8
"
            .to_vec(),
        );
        // The sound alone, as DASH: the init piece, then the unencrypted pieces 3 to 5.
        files.insert(
            "manifest.mpd".into(),
            br#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S"><Period>
  <AdaptationSet mimeType="audio/mp4">
    <SegmentTemplate timescale="1" duration="4" startNumber="3" initialization="init.mp4" media="p$Number$.m4s"/>
    <Representation id="a" bandwidth="64000"/>
  </AdaptationSet>
</Period></MPD>"#
                .to_vec(),
        );
        Self { files, expected, requests: Mutex::default(), delay }
    }

    async fn spawn(self) -> (Arc<Self>, String) {
        let server = Arc::new(self);
        let app = Router::new()
            .route("/v/{name}", axum::routing::get(serve))
            .route("/keys/{name}", axum::routing::get(key))
            .with_state(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v/master.m3u8", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (server, url)
    }

    fn requests(&self, name: &str) -> usize {
        self.requests.lock().get(name).copied().unwrap_or(0)
    }
}

async fn key(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    // The browser's headers go with every request, the key's too.
    assert_eq!(headers.get(header::REFERER).unwrap(), "https://site.example/watch");
    *server.requests.lock().entry("key".into()).or_default() += 1;
    Response::new(Body::from(KEY.to_vec()))
}

async fn serve(
    State(server): State<Arc<Server>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(headers.get(header::REFERER).unwrap(), "https://site.example/watch");
    *server.requests.lock().entry(name.clone()).or_default() += 1;
    let Some(file) = server.files.get(&name) else {
        return Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap();
    };
    if name.ends_with(".m4s") || name == "all.mp4" {
        tokio::time::sleep(server.delay).await;
    }
    let range = headers.get(header::RANGE).and_then(|v| {
        let (start, end) = v.to_str().ok()?.strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
    });
    match range {
        Some((start, end)) => Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", file.len()))
            .body(Body::from(file[start..=end].to_vec()))
            .unwrap(),
        None => Response::new(Body::from(file.clone())),
    }
}

fn open(dir: &Path) -> Engine {
    let config = Config {
        connections: 3,
        retry_delay: Duration::from_millis(20),
        progress_interval: Duration::from_millis(20),
        checkpoint_interval: Duration::from_millis(30),
        ..Config::default()
    };
    Engine::open(dir.join("state.redb"), config).unwrap()
}

fn video(url: &str, dir: &Path) -> NewDownload {
    let mut new = NewDownload::new(url, dir);
    new.media = Some(MediaFormat::Video { max_height: Some(1080) });
    new.file_name = Some("clip.mp4".into());
    new.headers = vec![("Referer".into(), "https://site.example/watch".into())];
    new
}

/// Only the finished file and the engine's database are left.
fn assert_only_the_video(dir: &Path) {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("state.redb"))
        .collect();
    names.sort();
    assert_eq!(names, ["clip.mp4"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_an_hls_stream_piece_by_piece() {
    let (server, url) = Server::new(Duration::ZERO).spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(video(&url, tmp.path())).await.unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert!(record.stream);
    assert_eq!(record.file_name, "clip.mp4");
    let saved = std::fs::read(tmp.path().join("clip.mp4")).unwrap();
    assert!(saved == server.expected, "the video differs from the stream's pieces");
    assert_only_the_video(tmp.path());
    assert_eq!(server.requests("360.m3u8"), 0);
    assert_eq!(server.requests("key"), 1);
    assert_eq!(server.requests("all.mp4"), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_stream_goes_on_from_the_next_piece() {
    let (server, url) = Server::new(Duration::from_millis(60)).spawn().await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());

    let id = engine.add(video(&url, tmp.path())).await.unwrap();
    let mut events = engine.subscribe();
    loop {
        match events.recv().await {
            Ok(Event::Progress(p)) if p.id == id && p.downloaded >= (3 * PIECE) as u64 => break,
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => unreachable!(),
        }
    }
    // Pieces that arrived are written only in order; wait until a few are on disk.
    let started = std::time::Instant::now();
    while engine.get(id).unwrap().tracks.first().is_none_or(|t| t.pieces < 3) {
        assert!(started.elapsed() < Duration::from_secs(10), "no pieces were written");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    engine.pause(id);
    let paused = engine.wait(id).await.unwrap();
    assert_eq!(paused.status, Status::Paused, "{:?}", paused.error);
    let written = paused.tracks[0].pieces;
    assert!(written >= 3, "only {written} pieces were written");
    let before: usize = (0..PIECES).map(|i| server.requests(&format!("p{i}.m4s"))).sum();

    engine.start(id).unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    let saved = std::fs::read(tmp.path().join("clip.mp4")).unwrap();
    assert!(saved == server.expected, "the video differs from the stream's pieces");
    assert_only_the_video(tmp.path());
    // The init piece and the pieces written before the pause were not fetched again: each
    // piece file not yet written was asked for once more.
    assert_eq!(server.requests("init.mp4"), 1);
    let after: usize = (0..PIECES).map(|i| server.requests(&format!("p{i}.m4s"))).sum();
    // The first piece is the init piece.
    let files_written = (written - 1).min(6);
    assert_eq!(after - before, 6 - files_written, "{before} then {after} requests");
}

/// Downloads the smallest quality of a public test stream, and checks that ffmpeg can read
/// the result. Run with `cargo test --test stream -- --ignored`, with ffmpeg installed.
async fn downloads_a_real_stream(url: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    let mut new = NewDownload::new(url, tmp.path());
    new.media = Some(MediaFormat::Video { max_height: Some(1) });
    new.file_name = Some("real.mp4".into());
    let id = engine.add(new).await.unwrap();
    let record = engine.wait(id).await.unwrap();
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    let path = record.path();
    println!("{} bytes in {}", record.total.unwrap(), path.display());
    let probe = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&probe.stderr);
    assert!(probe.status.success() && errors.trim().is_empty(), "ffmpeg: {errors}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the network and ffmpeg"]
async fn downloads_a_real_ts_stream() {
    downloads_a_real_stream("https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the network and ffmpeg"]
async fn downloads_a_real_stream_with_separate_sound() {
    downloads_a_real_stream(
        "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_the_sound_of_a_dash_stream() {
    let (server, url) = Server::new(Duration::ZERO).spawn().await;
    let url = url.replace("master.m3u8", "manifest.mpd");
    let tmp = tempfile::tempdir().unwrap();
    let engine = open(tmp.path());
    let mut new = video(&url, tmp.path());
    new.media = Some(MediaFormat::Audio);
    new.file_name = Some("clip.m4a".into());

    let id = engine.add(new).await.unwrap();
    let record = engine.wait(id).await.unwrap();

    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert_eq!(record.file_name, "clip.m4a");
    let mut expected = server.files["init.mp4"].clone();
    for i in 3..6 {
        expected.extend_from_slice(&server.files[&format!("p{i}.m4s")]);
    }
    let saved = std::fs::read(tmp.path().join("clip.m4a")).unwrap();
    assert!(saved == expected, "the sound differs from the stream's pieces");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the network and ffmpeg"]
async fn downloads_a_real_dash_stream() {
    downloads_a_real_stream("https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the network and ffmpeg"]
async fn downloads_a_real_dash_stream_in_whole_files() {
    downloads_a_real_stream(
        "https://dash.akamaized.net/dash264/TestCases/1a/netflix/exMPD_BIP_TC1.mpd",
    )
    .await;
}
