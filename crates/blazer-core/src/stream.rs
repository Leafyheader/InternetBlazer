//! Downloads HLS and DASH streams with the engine's own connections, as IDM does, instead of
//! handing them to yt-dlp. The pieces a playlist lists are fetched in parallel with the headers the browser
//! sent, decrypted where the playlist says so, and written in order to one file per track. A
//! video whose picture and sound come apart is then joined by ffmpeg, which copies the streams
//! without encoding them again. Pages without such a playlist, YouTube's among them, stay with
//! yt-dlp.
//!
//! Each track's progress is the number of pieces written, so a paused download goes on from the
//! next piece. Pieces that arrive ahead of their turn wait in memory, at most a few per
//! connection.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::{StatusCode, header};
use tokio::{
    io::AsyncWriteExt,
    sync::Notify,
    task::JoinSet,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    child::Tree,
    engine::{Inner, MAX_CONNECTIONS},
    error::{Error, Result},
    filename, fsio,
    limit::RateLimiter,
    media, playlist, probe,
    segment::Segment,
    tools,
    types::{self, DownloadRecord, Event, Progress, Status, TrackProgress},
};

/// Pieces are held in memory until their turn to be written; anything this large isn't one.
const MAX_PIECE: u64 = 256 * 1024 * 1024;
/// How many pieces per connection may be fetched ahead of the first one not yet written.
const AHEAD_PER_CONNECTION: usize = 2;
const SPEED_SMOOTHING: f64 = 0.3;
const MAX_BACKOFF_SHIFT: u32 = 5;

/// One stream to download in full: the picture (with its sound, unless that comes apart), or the
/// sound.
#[derive(Debug)]
pub(crate) struct Track {
    pub kind: TrackKind,
    pub container: Container,
    /// In order. An initialization piece, which fragmented MP4 starts with, lasts no time.
    pub pieces: Vec<Piece>,
    /// Bits per second, as the playlist states it, for a size estimate before pieces arrive.
    pub bandwidth: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackKind {
    Video,
    Audio,
}

/// What the pieces are, which decides what the joined file can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    /// Fragmented MP4: the pieces in order make a file that plays as it is.
    Mp4,
    /// MPEG-TS, which plays as it is too, but ffmpeg turns into MP4 when it is there.
    Ts,
    /// Bare AAC sound.
    Aac,
    /// WebM, from DASH, which also plays as it is.
    WebM,
}

#[derive(Debug, Clone)]
pub(crate) struct Piece {
    pub url: Url,
    /// Start and length, for a piece that is part of a larger file.
    pub range: Option<(u64, u64)>,
    pub key: Option<Key>,
    pub seconds: f64,
}

/// AES-128 in CBC mode, the only encryption HLS has without DRM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub url: Url,
    pub iv: [u8; 16],
}

/// State shared by the connections of one stream download.
struct Shared {
    inner: Arc<Inner>,
    limiter: Arc<RateLimiter>,
    headers: Vec<(String, String)>,
    tracks: Vec<Track>,
    /// The pieces still to fetch, as (track, piece), taken in order.
    jobs: Vec<(usize, usize)>,
    next_job: AtomicUsize,
    writers: Vec<tokio::sync::Mutex<Writer>>,
    /// What each track's file holds. Changed only by its writer, while holding it.
    written: Mutex<Vec<TrackProgress>>,
    /// Signalled when a track's file grows, so connections waiting to get ahead may.
    advanced: Notify,
    /// How many pieces a track may be fetched ahead of its first unwritten one.
    ahead: usize,
    /// Held while a key is fetched, so it is fetched once however many pieces need it.
    keys: tokio::sync::Mutex<HashMap<Url, [u8; 16]>>,
    stop: CancellationToken,
    /// Bytes received for pieces not yet written.
    pending: AtomicU64,
    /// Bytes received in all, which the reporter watches to tell a stall.
    received: AtomicU64,
    connections: AtomicUsize,
    throttled: AtomicUsize,
    trouble: Mutex<Option<String>>,
    max_retries: u32,
    retry_delay: std::time::Duration,
}

struct Writer {
    file: tokio::fs::File,
    /// The next piece to write.
    next: usize,
    /// Pieces that arrived before their turn, with the bytes received for each.
    ready: BTreeMap<usize, (Vec<u8>, u64)>,
}

/// Returns `Ok(true)` once complete and `Ok(false)` when paused, like the engine's other
/// downloads. Fails with [`Error::NotPlaylist`] for a stream better left to yt-dlp.
pub(crate) async fn execute(
    inner: &Arc<Inner>,
    record: &mut DownloadRecord,
    cancel: &CancellationToken,
    limiter: Arc<RateLimiter>,
) -> Result<bool> {
    let format = record.media.ok_or_else(|| Error::internal("not a media download"))?;
    record.status = Status::Downloading;
    record.error = None;
    record.last_try_at = Some(types::now());
    inner.save_running(record)?;
    inner.emit(Event::StatusChanged { id: record.id, status: record.status, error: None });

    let url = Url::parse(&record.url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    // The playlist is read each time: its addresses may hold tokens that expire.
    let tracks = tokio::select! {
        tracks = playlist::tracks(&inner.client, &url, &record.headers, format) => tracks?,
        () = cancel.cancelled() => return Ok(false),
    };
    let ffmpeg = tools::ffmpeg(inner.config.tools_dir.as_deref());
    if tracks.len() > 1 && ffmpeg.is_none() {
        return Err(Error::Media {
            message: "This video needs ffmpeg to join its picture and sound. Install ffmpeg (on \
                      Windows: winget install ffmpeg) and try again."
                .into(),
            retryable: false,
        });
    }

    let progress = resume_point(record, &tracks);
    record.tracks = progress.clone();
    if progress.iter().all(TrackProgress::is_complete) {
        return join(inner, record, &tracks, ffmpeg.as_deref(), cancel).await;
    }
    let paths: Vec<PathBuf> = tracks.iter().map(|t| track_path(record, t.kind)).collect();
    let mut writers = Vec::new();
    for (path, p) in paths.iter().zip(&progress) {
        let (path, len) = (path.clone(), p.bytes);
        let file = tokio::task::spawn_blocking(move || open_track(&path, len))
            .await
            .map_err(Error::internal)??;
        writers.push(tokio::sync::Mutex::new(Writer {
            file: tokio::fs::File::from_std(file),
            next: p.pieces,
            ready: BTreeMap::new(),
        }));
    }
    let jobs: Vec<(usize, usize)> = tracks
        .iter()
        .enumerate()
        .flat_map(|(t, track)| (progress[t].pieces..track.pieces.len()).map(move |i| (t, i)))
        .collect();
    let worker_count = record
        .connections
        .unwrap_or_else(|| inner.connections())
        .clamp(1, MAX_CONNECTIONS)
        .min(jobs.len().max(1));
    let cfg = &inner.config;
    let sh = Arc::new(Shared {
        inner: inner.clone(),
        limiter,
        headers: record.headers.clone(),
        tracks,
        jobs,
        next_job: AtomicUsize::new(0),
        writers,
        written: Mutex::new(progress),
        advanced: Notify::new(),
        ahead: worker_count * AHEAD_PER_CONNECTION,
        keys: tokio::sync::Mutex::default(),
        stop: cancel.child_token(),
        pending: AtomicU64::new(0),
        received: AtomicU64::new(0),
        connections: AtomicUsize::new(0),
        throttled: AtomicUsize::new(0),
        trouble: Mutex::default(),
        max_retries: cfg.max_retries,
        retry_delay: cfg.retry_delay,
    });

    let mut workers = JoinSet::new();
    for _ in 0..worker_count {
        workers.spawn(worker(sh.clone()));
    }
    let mut ticker = time::interval(cfg.progress_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_checkpoint = Instant::now();
    let mut last_sample = (Instant::now(), downloaded(&sh));
    let mut speed = 0.0;
    let (mut received, mut last_data) = (0, Instant::now());
    let mut failure = None;
    loop {
        tokio::select! {
            joined = workers.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                // A piece that can't be had stops the rest, which can't be written past it.
                Some(Ok(Err(e))) => {
                    failure.get_or_insert(e);
                    sh.stop.cancel();
                }
                Some(Err(e)) => {
                    failure.get_or_insert(Error::internal(e));
                    sh.stop.cancel();
                }
            },
            _ = ticker.tick() => {
                let now = Instant::now();
                let done = downloaded(&sh);
                let elapsed = now.duration_since(last_sample.0).as_secs_f64();
                if elapsed > 0.0 {
                    let sample = done.saturating_sub(last_sample.1) as f64 / elapsed;
                    speed = match speed == 0.0 {
                        true => sample,
                        false => SPEED_SMOOTHING * sample + (1.0 - SPEED_SMOOTHING) * speed,
                    };
                }
                last_sample = (now, done);
                let now_received = sh.received.load(Ordering::Relaxed);
                if now_received != received || sh.throttled.load(Ordering::Relaxed) > 0 {
                    received = now_received;
                    last_data = now;
                    *sh.trouble.lock() = None;
                }
                let stalled = now.duration_since(last_data) >= cfg.stall_after;
                let total = estimate(&sh).map(|t| t.max(done));
                inner.emit(Event::Progress(Progress {
                    id: record.id,
                    downloaded: done,
                    total,
                    speed: speed as u64,
                    connections: sh.connections.load(Ordering::Relaxed),
                    planned: worker_count,
                    refused: 0,
                    idle: 0,
                    stalled,
                    problem: if stalled { sh.trouble.lock().clone() } else { None },
                    segments: vec![Segment { start: 0, end: total.unwrap_or(done), downloaded: done }],
                }));
                if now.duration_since(last_checkpoint) >= cfg.checkpoint_interval {
                    last_checkpoint = now;
                    save_progress(record, &sh).await;
                    let (inner, mut record) = (inner.clone(), record.clone());
                    match tokio::task::spawn_blocking(move || inner.save_running(&mut record)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!("checkpoint failed: {e}"),
                        Err(e) => tracing::warn!("checkpoint task failed: {e}"),
                    }
                }
            }
        }
    }
    save_progress(record, &sh).await;
    // Also closes the files, which Windows requires before ffmpeg reads them or they are renamed.
    let Shared { tracks, .. } = Arc::into_inner(sh).expect("the connections have finished");
    if let Some(e) = failure {
        return Err(e);
    }
    if record.tracks.iter().all(TrackProgress::is_complete) {
        return join(inner, record, &tracks, ffmpeg.as_deref(), cancel).await;
    }
    if cancel.is_cancelled() {
        return Ok(false);
    }
    Err(Error::Incomplete)
}

/// Where the last run left off, if the playlist still has the same pieces and the files hold
/// what was recorded; otherwise the start.
fn resume_point(record: &DownloadRecord, tracks: &[Track]) -> Vec<TrackProgress> {
    let fresh = || {
        tracks
            .iter()
            .map(|t| TrackProgress { pieces: 0, total_pieces: t.pieces.len(), bytes: 0 })
            .collect()
    };
    let same = record.tracks.len() == tracks.len()
        && record.tracks.iter().zip(tracks).all(|(p, t)| {
            p.total_pieces == t.pieces.len()
                && std::fs::metadata(track_path(record, t.kind)).is_ok_and(|m| m.len() >= p.bytes)
        });
    if !same && record.tracks.iter().any(|p| p.pieces > 0) {
        tracing::info!(id = %record.id, "the stream changed or its files are gone; starting over");
    }
    if same { record.tracks.clone() } else { fresh() }
}

/// The file that collects a track's pieces. It counts among yt-dlp's working files (see
/// [`media::working_files`]), so moving and removing the download handle it too.
fn track_path(record: &DownloadRecord, kind: TrackKind) -> PathBuf {
    let stem = media::stem(&record.file_name);
    let name = match kind {
        TrackKind::Video => format!("{stem}.video.part"),
        TrackKind::Audio => format!("{stem}.audio.part"),
    };
    record.dir.join(name)
}

/// Opens a track's file to append to, cut to `len`: anything after it was written after the
/// last checkpoint and may be incomplete.
fn open_track(path: &Path, len: u64) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
    file.set_len(len)?;
    drop(file);
    std::fs::OpenOptions::new().append(true).open(path)
}

/// Bytes on disk and received for pieces not yet written.
fn downloaded(sh: &Shared) -> u64 {
    let written: u64 = sh.written.lock().iter().map(|p| p.bytes).sum();
    written + sh.pending.load(Ordering::Relaxed)
}

/// The size of the whole download: exact for pieces that state their length, otherwise from how
/// many bytes per second the pieces so far took, or from the bitrate the playlist states.
fn estimate(sh: &Shared) -> Option<u64> {
    let written = sh.written.lock().clone();
    let mut total = 0;
    for (track, done) in sh.tracks.iter().zip(&written) {
        let seconds: f64 = track.pieces.iter().map(|p| p.seconds).sum();
        let seconds_done: f64 = track.pieces[..done.pieces].iter().map(|p| p.seconds).sum();
        total += if let Some(lengths) =
            track.pieces.iter().map(|p| p.range.map(|(_, len)| len)).sum::<Option<u64>>()
        {
            lengths
        } else if seconds_done > 0.0 && done.bytes > 0 {
            (done.bytes as f64 / seconds_done * seconds) as u64
        } else {
            (track.bandwidth? as f64 * seconds / 8.0) as u64
        };
    }
    Some(total)
}

/// Copies what the files hold into the record, once it is on disk.
async fn save_progress(record: &mut DownloadRecord, sh: &Shared) {
    for (t, writer) in sh.writers.iter().enumerate() {
        let mut writer = writer.lock().await;
        let synced = async {
            writer.file.flush().await?;
            writer.file.sync_data().await
        };
        if let Err(e) = synced.await {
            // The progress saved before stays; what came since is fetched again.
            tracing::warn!(id = %record.id, "could not save a track's file: {e}");
            continue;
        }
        record.tracks[t] = sh.written.lock()[t];
    }
    let done = sh.written.lock().iter().map(|p| p.bytes).sum();
    record.total = estimate(sh).map(|t| t.max(done));
    record.segments =
        vec![Segment { start: 0, end: record.total.unwrap_or(done), downloaded: done }];
}

/// One connection: fetches pieces in turn until none are left.
async fn worker(sh: Arc<Shared>) -> Result<()> {
    while let Some(&(t, i)) = sh.jobs.get(sh.next_job.fetch_add(1, Ordering::Relaxed)) {
        // Far ahead of what can be written, the piece would only take up memory.
        loop {
            let advanced = sh.advanced.notified();
            if i < sh.written.lock()[t].pieces + sh.ahead {
                break;
            }
            tokio::select! {
                () = advanced => {}
                () = sh.stop.cancelled() => return Ok(()),
            }
        }
        let Some((data, received)) = fetch_with_retries(&sh, t, i).await? else {
            return Ok(());
        };
        write(&sh, t, i, data, received).await?;
    }
    Ok(())
}

/// The piece's bytes, decrypted, and how many were received; `None` once the download stops.
async fn fetch_with_retries(sh: &Shared, t: usize, i: usize) -> Result<Option<(Vec<u8>, u64)>> {
    let mut failures = 0;
    loop {
        let result = tokio::select! {
            result = fetch(sh, &sh.tracks[t].pieces[i]) => result,
            () = sh.stop.cancelled() => return Ok(None),
        };
        match result {
            Ok(piece) => return Ok(Some(piece)),
            Err(e) if e.is_retryable() && failures < sh.max_retries => {
                failures += 1;
                *sh.trouble.lock() = Some(e.to_string());
                let delay = e.retry_after().unwrap_or_else(|| {
                    sh.retry_delay * (1 << (failures - 1).min(MAX_BACKOFF_SHIFT))
                });
                tracing::debug!("retrying a piece in {delay:?} after: {e}");
                tokio::select! {
                    () = time::sleep(delay) => {}
                    () = sh.stop.cancelled() => return Ok(None),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Downloads one piece into memory and decrypts it. Bytes received count as pending until the
/// piece is written, or are taken back if it fails.
async fn fetch(sh: &Shared, piece: &Piece) -> Result<(Vec<u8>, u64)> {
    let mut got = 0;
    let result: Result<Vec<u8>> = async {
        let mut data = receive(sh, piece, &mut got).await?;
        if let Some(key) = &piece.key {
            let secret = key_bytes(sh, &key.url).await?;
            let len = cbc::Decryptor::<aes::Aes128>::new(&secret.into(), &key.iv.into())
                .decrypt_padded_mut::<Pkcs7>(&mut data)
                .map_err(|_| {
                    Error::Invalid("a piece of the stream did not decrypt with its key".into())
                })?
                .len();
            data.truncate(len);
        }
        Ok(data)
    }
    .await;
    if result.is_err() {
        sh.pending.fetch_sub(got, Ordering::Relaxed);
    }
    Ok((result?, got))
}

async fn receive(sh: &Shared, piece: &Piece, got: &mut u64) -> Result<Vec<u8>> {
    let mut req = probe::request(&sh.inner.client, piece.url.as_str(), &sh.headers);
    if let Some((start, len)) = piece.range {
        req = req.header(header::RANGE, format!("bytes={start}-{}", start + len - 1));
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::from_response(status, resp.headers()));
    }
    // A whole file where a part of it was asked for.
    if piece.range.is_some() && status != StatusCode::PARTIAL_CONTENT {
        return Err(Error::RangeIgnored);
    }
    let expected = piece.range.map(|(_, len)| len).or(resp.content_length());
    if expected.is_some_and(|len| len > MAX_PIECE) {
        return Err(Error::Invalid("a piece of the stream is too large".into()));
    }
    let _connection = Counted::new(&sh.connections);
    let mut data = Vec::with_capacity(expected.unwrap_or(0) as usize);
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        data.extend_from_slice(&chunk);
        if data.len() as u64 > MAX_PIECE {
            return Err(Error::Invalid("a piece of the stream is too large".into()));
        }
        // More than the part asked for: the server sent something else.
        if piece.range.is_some_and(|(_, len)| data.len() as u64 > len) {
            return Err(Error::RangeIgnored);
        }
        let n = chunk.len() as u64;
        *got += n;
        sh.pending.fetch_add(n, Ordering::Relaxed);
        sh.received.fetch_add(n, Ordering::Relaxed);
        if n > 0 {
            let _throttled = Counted::new(&sh.throttled);
            sh.inner.speed_limit.take(chunk.len()).await;
            sh.limiter.take(chunk.len()).await;
        }
    }
    if expected.is_some_and(|len| data.len() as u64 != len) {
        return Err(Error::Truncated);
    }
    Ok(data)
}

/// The 16-byte key at `url`, fetched once per download: some servers give a key only once.
async fn key_bytes(sh: &Shared, url: &Url) -> Result<[u8; 16]> {
    let mut keys = sh.keys.lock().await;
    if let Some(key) = keys.get(url) {
        return Ok(*key);
    }
    let resp = probe::request(&sh.inner.client, url.as_str(), &sh.headers).send().await?;
    if !resp.status().is_success() {
        return Err(Error::from_response(resp.status(), resp.headers()));
    }
    let bytes = resp.bytes().await?;
    let key: [u8; 16] = bytes.as_ref().try_into().map_err(|_| {
        Error::Invalid(format!("the stream's key has {} bytes instead of 16", bytes.len()))
    })?;
    keys.insert(url.clone(), key);
    Ok(key)
}

/// Writes the piece once those before it are written, and with it any after it that came early.
async fn write(sh: &Shared, t: usize, i: usize, data: Vec<u8>, received: u64) -> Result<()> {
    let mut writer = sh.writers[t].lock().await;
    writer.ready.insert(i, (data, received));
    loop {
        let next = writer.next;
        let Some((data, received)) = writer.ready.remove(&next) else { break };
        writer.file.write_all(&data).await?;
        writer.next += 1;
        sh.pending.fetch_sub(received, Ordering::Relaxed);
        let mut written = sh.written.lock();
        written[t].pieces = writer.next;
        written[t].bytes += data.len() as u64;
    }
    drop(writer);
    sh.advanced.notify_waiters();
    Ok(())
}

/// Turns the finished tracks into the video: a fragmented MP4 track is one as it is, MPEG-TS
/// becomes MP4 when ffmpeg is there, and picture and sound apart are joined into one file.
async fn join(
    inner: &Arc<Inner>,
    record: &mut DownloadRecord,
    tracks: &[Track],
    ffmpeg: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let parts: Vec<PathBuf> = tracks.iter().map(|t| track_path(record, t.kind)).collect();
    let stem = media::stem(&record.file_name).to_string();
    let (ext, remux) = output(tracks, ffmpeg.is_some());
    let done = match (remux, ffmpeg) {
        (true, Some(ffmpeg)) => {
            let temp = record.dir.join(format!("{stem}.temp.{ext}"));
            let joined = tokio::select! {
                joined = run_ffmpeg(ffmpeg, &parts, &temp, ext) => joined,
                () = cancel.cancelled() => Ok(false),
            };
            match joined {
                Ok(true) => temp,
                Ok(false) => {
                    let _ = std::fs::remove_file(&temp);
                    return Ok(false);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&temp);
                    return Err(e);
                }
            }
        }
        _ => parts[0].clone(),
    };
    record.file_name = format!("{stem}.{ext}");
    // Never over another file: if one took the name, pick a new one.
    loop {
        if record.path().exists() {
            record.file_name = filename::unique(&record.dir, &record.file_name, |p| p.exists());
        }
        match fsio::rename_new(&done, &record.path()) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            result => result?,
        }
        break;
    }
    if remux {
        for part in &parts {
            if let Err(e) = std::fs::remove_file(part) {
                tracing::debug!(id = %record.id, "could not remove {}: {e}", part.display());
            }
        }
    }
    let len = std::fs::metadata(record.path())?.len();
    record.total = Some(len);
    record.segments = vec![Segment { start: 0, end: len, downloaded: len }];
    inner.save_running(record)?;
    Ok(true)
}

/// The extension of the finished file, and whether ffmpeg makes it.
fn output(tracks: &[Track], ffmpeg: bool) -> (&'static str, bool) {
    match tracks {
        [track] => match (track.container, track.kind) {
            (Container::Mp4, TrackKind::Video) => ("mp4", false),
            (Container::Mp4, TrackKind::Audio) => ("m4a", false),
            (Container::Ts, TrackKind::Video) if ffmpeg => ("mp4", true),
            (Container::Ts | Container::Aac, TrackKind::Audio) if ffmpeg => ("m4a", true),
            (Container::Aac, _) => ("aac", false),
            (Container::Ts, _) => ("ts", false),
            (Container::WebM, _) => ("webm", false),
        },
        // MP4 can't hold everything WebM can.
        _ if tracks.iter().any(|t| t.container == Container::WebM) => ("mkv", true),
        _ => ("mp4", true),
    }
}

/// Joins or converts `inputs` into `output` without encoding again. Returns whether it worked;
/// dropping the future stops ffmpeg.
async fn run_ffmpeg(ffmpeg: &Path, inputs: &[PathBuf], output: &Path, ext: &str) -> Result<bool> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]).stdin(Stdio::null());
    for input in inputs {
        cmd.arg("-i").arg(input);
    }
    if inputs.len() > 1 {
        cmd.args(["-map", "0:v:0", "-map", "1:a:0"]);
    }
    cmd.args(["-c", "copy"]);
    if matches!(ext, "mp4" | "m4a") {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(output);
    let out = Tree::output(&mut cmd).await.map_err(|e| Error::Media {
        message: format!("could not start ffmpeg: {e}"),
        retryable: false,
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("no reason given");
        return Err(Error::Media {
            message: format!("ffmpeg could not put the video together: {}", reason.trim()),
            retryable: false,
        });
    }
    Ok(true)
}

/// Counts something as going on while it lives.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(kind: TrackKind, container: Container) -> Track {
        Track { kind, container, pieces: Vec::new(), bandwidth: None }
    }

    #[test]
    fn picks_the_finished_file_type() {
        let video = |c| output(&[track(TrackKind::Video, c)], false);
        assert_eq!(video(Container::Mp4), ("mp4", false));
        assert_eq!(video(Container::Ts), ("ts", false));
        assert_eq!(output(&[track(TrackKind::Video, Container::Ts)], true), ("mp4", true));
        assert_eq!(output(&[track(TrackKind::Audio, Container::Aac)], true), ("m4a", true));
        let both =
            [track(TrackKind::Video, Container::Mp4), track(TrackKind::Audio, Container::Mp4)];
        assert_eq!(output(&both, true), ("mp4", true));
        let webm =
            [track(TrackKind::Video, Container::WebM), track(TrackKind::Audio, Container::Mp4)];
        assert_eq!(output(&webm, true), ("mkv", true));
    }
}
