//! Videos and sounds on web pages, downloaded by yt-dlp running as a child process.
//!
//! yt-dlp knows how to get the media out of thousands of sites and fetches it itself, so the
//! engine only starts it, reads its progress and stops it. Pausing kills it; it carries on from
//! its `.part` files when it runs again. Sites like YouTube keep the picture and the sound in
//! separate files, which yt-dlp downloads one after the other and joins with ffmpeg.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    time::{self, Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::{
    child::Tree,
    engine::Inner,
    error::{Error, Result},
    filename,
    limit::RateLimiter,
    segment::Segment,
    tools,
    types::{DownloadRecord, Event, MediaChoice, MediaFormat, MediaInfo, Progress, Status},
};

/// Prefixes the lines the engine asks yt-dlp to print, to tell them from anything else.
const SIZE: &str = "BLAZER-SIZE ";
const PROGRESS: &str = "BLAZER-PROGRESS ";
const FILE: &str = "BLAZER-FILE ";
/// Most sites split their videos into fragments, which this many connections fetch at once.
const MAX_FRAGMENT_CONNECTIONS: usize = 8;

/// Finds the videos on a page with `yt-dlp -J`, which prints what it found as JSON.
pub(crate) async fn inspect(
    ytdlp: &Path,
    tools_dir: Option<&Path>,
    ffmpeg: bool,
    url: &str,
    headers: &[(String, String)],
) -> Result<MediaInfo> {
    let mut cmd = tools::ytdlp_command(ytdlp, tools_dir);
    cmd.args(["--ignore-config", "--no-playlist", "--encoding", "utf-8", "-J"])
        .args(header_args(headers))
        .arg("--")
        .arg(url)
        .stdin(Stdio::null());
    let output = Tree::output(&mut cmd).await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(failure(stderr.lines(), output.status.code()));
    }
    let info: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::internal(format!("reading yt-dlp's answer: {e}")))?;
    Ok(MediaInfo {
        title: info["title"].as_str().unwrap_or("video").to_string(),
        site: info["extractor_key"].as_str().unwrap_or_default().to_string(),
        duration: info["duration"].as_f64(),
        choices: choices(&info, ffmpeg),
        ffmpeg,
    })
}

/// A format as `yt-dlp -J` lists it.
#[derive(Debug)]
struct Format<'a> {
    ext: &'a str,
    video: bool,
    audio: bool,
    height: Option<u32>,
    fps: Option<f64>,
    size: Option<u64>,
    /// Bits per second, which ranks formats of the same kind.
    rate: f64,
}

impl<'a> Format<'a> {
    fn parse(f: &'a Value) -> Self {
        // A codec yt-dlp doesn't know is missing rather than `none`; it counts as present.
        let has = |key: &str| f[key].as_str() != Some("none");
        Self {
            ext: f["ext"].as_str().unwrap_or_default(),
            video: has("vcodec"),
            audio: has("acodec"),
            height: f["height"].as_u64().and_then(|h| u32::try_from(h).ok()),
            fps: f["fps"].as_f64(),
            size: f["filesize"].as_u64().or_else(|| f["filesize_approx"].as_u64()),
            rate: f["tbr"].as_f64().unwrap_or_default(),
        }
    }

    /// Ranks formats as the arguments in [`selection`] make yt-dlp do: MP4 and M4A before
    /// others, then higher frame rates and bit rates.
    fn rank(&self, preferred: &str) -> (bool, u64, u64) {
        (self.ext == preferred, self.fps.unwrap_or_default() as u64, self.rate as u64)
    }
}

/// The qualities to offer: one per picture height, best first, and the sound alone.
fn choices(info: &Value, ffmpeg: bool) -> Vec<MediaChoice> {
    // A page with one file lists no formats; the page itself describes it.
    let formats: Vec<Format> = match info["formats"].as_array() {
        Some(formats) => formats.iter().map(Format::parse).collect(),
        None => vec![Format::parse(info)],
    };
    let best = |pick: &dyn Fn(&Format) -> bool, preferred: &str| {
        formats.iter().filter(|f| pick(f)).max_by(|a, b| a.rank(preferred).cmp(&b.rank(preferred)))
    };
    let sound = best(&|f| f.audio && !f.video, "m4a");
    // Without ffmpeg, picture and sound cannot be joined, so only formats with both will do.
    let usable = |f: &Format| f.video && (ffmpeg || f.audio);

    let mut heights: Vec<u32> =
        formats.iter().filter(|f| usable(f)).filter_map(|f| f.height).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    let mut choices: Vec<MediaChoice> = heights
        .into_iter()
        .filter_map(|height| {
            let video = best(&|f| usable(f) && f.height == Some(height), "mp4")?;
            let fps = video
                .fps
                .filter(|&fps| fps > 30.0)
                .map_or(String::new(), |fps| format!("{fps:.0}"));
            let joined = (!video.audio).then_some(sound).flatten();
            let (ext, size) = match joined {
                Some(a) => (
                    if video.ext == "mp4" && a.ext == "m4a" { "mp4" } else { "mkv" },
                    video.size.zip(a.size).map(|(v, a)| v + a),
                ),
                None => (video.ext, video.size),
            };
            Some(MediaChoice {
                label: format!("{height}p{fps}"),
                format: MediaFormat::Video { max_height: Some(height) },
                ext: ext.to_string(),
                size,
            })
        })
        .collect();
    if choices.is_empty() && formats.iter().any(&usable) {
        let video = best(&|f| usable(f), "mp4");
        choices.push(MediaChoice {
            label: "Best quality".into(),
            format: MediaFormat::Video { max_height: None },
            ext: video.map_or("mp4", |v| v.ext).to_string(),
            size: video.and_then(|v| v.size),
        });
    }
    if let Some(sound) = sound {
        choices.push(MediaChoice {
            label: "Audio only".into(),
            format: MediaFormat::Audio,
            ext: sound.ext.to_string(),
            size: sound.size,
        });
    }
    choices
}

/// A file name to start with, from the page's title and the likely extension of `format`.
pub(crate) fn guess_name(info: &MediaInfo, format: MediaFormat) -> String {
    let ext = info
        .choices
        .iter()
        .find(|c| c.format == format)
        .or_else(|| info.choices.first())
        .map_or("mp4", |c| c.ext.as_str());
    // A title is no path, though it may hold slashes, as in "AC/DC".
    let title = info.title.replace(['/', '\\'], "_");
    filename::sanitize(&format!("{title}.{ext}"))
}

/// The name without its extension. yt-dlp's files all start with it, followed by a dot.
pub(crate) fn stem(file_name: &str) -> &str {
    file_name.rsplit_once('.').map_or(file_name, |(stem, _)| stem)
}

/// Whether a file that starts with `stem` and a dot is one yt-dlp works on, given the rest of its
/// name: `x.mp4.part`, the formats it joins (`x.f137.mp4`), their fragments
/// (`x.f137.mp4.part-Frag3`) and its notes on them (`x.mp4.ytdl`), or a file being joined
/// (`x.temp.mp4`).
fn is_working_file(rest: &str) -> bool {
    let format_part = rest.split_once('.').is_some_and(|(id, _)| {
        id.strip_prefix('f').is_some_and(|id| {
            id.chars().any(|c| c.is_ascii_digit())
                && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
        })
    });
    rest.ends_with(".part")
        || rest.contains(".part-Frag")
        || rest.ends_with(".ytdl")
        || rest.starts_with("temp.")
        || format_part
}

/// The unfinished files of a media download, which yt-dlp continues from.
pub(crate) fn working_files(record: &DownloadRecord) -> Vec<PathBuf> {
    let prefix = format!("{}.", stem(&record.file_name));
    let Ok(entries) = std::fs::read_dir(&record.dir) else { return Vec::new() };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix(&prefix).is_some_and(is_working_file)
        })
        .map(|entry| entry.path())
        .collect()
}

/// Whether some file in `dir` already uses the stem of `path`, so yt-dlp's files for it would
/// mix with that one's.
pub(crate) fn stem_taken(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return false };
    let prefix = format!("{}.", stem(&name.to_string_lossy()));
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with(&prefix))
    })
}

/// Whether the two files' names are the same but for the extension.
pub(crate) fn same_stem(a: &Path, b: &Path) -> bool {
    let stem_of = |p: &Path| p.file_name().map(|n| stem(&n.to_string_lossy()).to_string());
    stem_of(a) == stem_of(b)
}

fn header_args(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .flat_map(|(name, value)| ["--add-header".into(), format!("{name}:{value}")])
        .collect()
}

/// The yt-dlp formats to download, and how to rank them. Ranking by extension right after
/// the height makes it pick MP4 with M4A, which join into an MP4 file that plays everywhere.
fn selection(format: MediaFormat, ffmpeg: bool) -> (&'static str, String) {
    const PREFER: &str = "ext:mp4:m4a";
    match format {
        MediaFormat::Video { max_height } => (
            if ffmpeg { "bv*+ba/b" } else { "b" },
            match max_height {
                Some(height) => format!("res:{height},{PREFER}"),
                None => format!("res,{PREFER}"),
            },
        ),
        MediaFormat::Audio => ("ba/b", PREFER.into()),
    }
}

/// In yt-dlp's output templates, `%` starts a field.
fn escape_template(text: &str) -> String {
    text.replace('%', "%%")
}

fn args(
    record: &DownloadRecord,
    format: MediaFormat,
    ffmpeg: Option<&Path>,
    rate: Option<u64>,
    connections: usize,
) -> Vec<OsString> {
    let (formats, sort) = selection(format, ffmpeg.is_some());
    let mut output = OsString::from(escape_template(&record.dir.to_string_lossy()));
    output.push(std::path::MAIN_SEPARATOR_STR);
    output.push(escape_template(stem(&record.file_name)));
    output.push(".%(ext)s");
    let mut args: Vec<OsString> = [
        "--ignore-config",
        "--no-playlist",
        "--encoding",
        "utf-8",
        "--newline",
        "--progress",
        // The file's date is when it was downloaded, as with other downloads.
        "--no-mtime",
        "--progress-template",
        "download:BLAZER-PROGRESS %(info.format_id)s %(progress.status)s \
         %(progress.downloaded_bytes)s %(progress.total_bytes)s \
         %(progress.total_bytes_estimate)s %(progress.speed)s",
        "--print",
        "before_dl:BLAZER-SIZE %(filesize,filesize_approx)s",
        "--print",
        "after_move:BLAZER-FILE %(filepath)s",
        // `--print` alone would only pretend to download.
        "--no-simulate",
        "-f",
        formats,
        "-S",
        sort.as_str(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(ffmpeg) = ffmpeg {
        args.extend(["--merge-output-format".into(), "mp4/mkv".into()]);
        args.extend(["--ffmpeg-location".into(), ffmpeg.as_os_str().to_owned()]);
    }
    args.extend(["-N".into(), connections.to_string().into()]);
    if let Some(rate) = rate {
        args.extend(["--limit-rate".into(), rate.to_string().into()]);
    }
    args.extend(header_args(&record.headers).into_iter().map(OsString::from));
    args.extend(["-o".into(), output, "--".into(), record.url.clone().into()]);
    args
}

/// Turns what yt-dlp printed when it failed into an error, which may be worth retrying when it
/// sounds like a network problem.
fn failure<'a>(lines: impl Iterator<Item = &'a str>, code: Option<i32>) -> Error {
    let lines: Vec<&str> = lines.collect();
    let Some(last) = lines.iter().rev().find_map(|l| l.trim().strip_prefix("ERROR:")) else {
        let code = code.map_or("none".into(), |c| c.to_string());
        return Error::Media {
            message: format!("yt-dlp stopped unexpectedly (exit code {code})"),
            retryable: true,
        };
    };
    let message = without_site(last.trim()).to_string();
    let lower = message.to_ascii_lowercase();
    let client_error = lower.contains("http error 4") && !lower.contains("http error 429");
    let retryable = !client_error
        && [
            "timed out",
            "timeout",
            "connection",
            "network",
            "temporary failure",
            "getaddrinfo",
            "unreachable",
            "http error 5",
            "http error 429",
            "incomplete",
            "unable to download",
            "reset by peer",
        ]
        .iter()
        .any(|sign| lower.contains(sign));
    Error::Media { message, retryable }
}

/// Explains yt-dlp's "Requested format is not available", whose advice is for its own users.
fn missing_format(ffmpeg: bool) -> &'static str {
    match ffmpeg {
        false => {
            "this site keeps picture and sound apart, and joining them needs ffmpeg, which is not \
             installed. Install ffmpeg and restart the download, or download the audio only"
        }
        true => {
            "the site does not offer this quality (any more); add the download again to pick another"
        }
    }
}

/// Drops the `[youtube] jNQXAC9IVRw: ` that starts yt-dlp's messages about a video.
fn without_site(message: &str) -> &str {
    let Some(rest) = message.strip_prefix('[').and_then(|m| m.split_once("] ")).map(|(_, r)| r)
    else {
        return message;
    };
    match rest.split_once(": ") {
        Some((id, text)) if !id.contains(' ') => text,
        _ => rest,
    }
}

/// Follows yt-dlp's progress through the files it downloads one after another.
#[derive(Debug, Default)]
struct Tracker {
    /// The size of the whole download, as yt-dlp estimates it before starting.
    estimate: Option<u64>,
    /// Bytes of the files already finished.
    done: u64,
    /// The format being downloaded now.
    stage: Option<String>,
    stage_bytes: u64,
    stage_total: Option<u64>,
    /// Still receiving data, rather than joining files or between them.
    receiving: bool,
    speed: u64,
    file: Option<PathBuf>,
}

impl Tracker {
    /// Takes in a line of yt-dlp's output. Returns whether new data arrived.
    fn line(&mut self, line: &str) -> bool {
        let line = line.trim_end();
        if let Some(size) = line.strip_prefix(SIZE) {
            self.estimate = number(size);
        } else if let Some(path) = line.strip_prefix(FILE) {
            self.file = Some(PathBuf::from(path));
        } else if let Some(progress) = line.strip_prefix(PROGRESS) {
            let fields: Vec<&str> = progress.split(' ').collect();
            let &[stage, status, downloaded, total, estimate, speed] = fields.as_slice() else {
                return false;
            };
            if self.stage.as_deref() != Some(stage) {
                if self.stage.is_some() {
                    self.done += self.stage_total.unwrap_or(0).max(self.stage_bytes);
                }
                self.stage = Some(stage.to_string());
                self.stage_bytes = 0;
            }
            self.stage_total = number(total).or_else(|| number(estimate));
            // A file finished before reports its size but no bytes.
            let downloaded = number(downloaded)
                .or(if status == "finished" { self.stage_total } else { None })
                .unwrap_or(self.stage_bytes);
            let arrived = downloaded != self.stage_bytes;
            self.stage_bytes = downloaded;
            self.receiving = status == "downloading";
            self.speed = if self.receiving { number(speed).unwrap_or(0) } else { 0 };
            return arrived;
        }
        false
    }

    fn downloaded(&self) -> u64 {
        self.done + self.stage_bytes
    }

    fn total(&self) -> Option<u64> {
        let total = self.estimate.or_else(|| self.stage_total.map(|t| self.done + t))?;
        Some(total.max(self.downloaded()))
    }

    fn segment(&self) -> Segment {
        let downloaded = self.downloaded();
        Segment { start: 0, end: self.total().unwrap_or(downloaded), downloaded }
    }
}

/// yt-dlp prints `NA` for what it doesn't know, and some sizes as fractions.
fn number(field: &str) -> Option<u64> {
    field.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64)
}

/// The tightest of the two speed limits; yt-dlp takes one number.
fn rate(global: &RateLimiter, own: &RateLimiter) -> Option<u64> {
    match (global.limit(), own.limit()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

enum Ended {
    Finished,
    Stopped,
    /// yt-dlp cannot change its speed limit while it runs, so it is started again.
    LimitChanged,
}

/// Returns `Ok(true)` once complete and `Ok(false)` when paused, like the engine's own downloads.
pub(crate) async fn execute(
    inner: &Arc<Inner>,
    record: &mut DownloadRecord,
    cancel: &CancellationToken,
    limiter: Arc<RateLimiter>,
) -> Result<bool> {
    let format = record.media.ok_or_else(|| Error::internal("not a media download"))?;
    record.status = Status::Downloading;
    record.error = None;
    inner.save_running(record)?;
    inner.emit(Event::StatusChanged { id: record.id, status: record.status, error: None });

    let tools_dir = inner.config.tools_dir.as_deref();
    let (ytdlp, _) = tools::ytdlp(tools_dir).ok_or(Error::NoYtdlp)?;
    let ffmpeg = tools::ffmpeg(tools_dir);
    loop {
        match attempt(inner, record, cancel, &limiter, &ytdlp, ffmpeg.as_deref(), format).await? {
            Ended::Finished => return Ok(true),
            Ended::Stopped => return Ok(false),
            Ended::LimitChanged => {
                tracing::debug!(id = %record.id, "restarting yt-dlp at the new speed limit")
            }
        }
    }
}

/// Runs yt-dlp once, until it ends or the download stops.
async fn attempt(
    inner: &Arc<Inner>,
    record: &mut DownloadRecord,
    cancel: &CancellationToken,
    limiter: &RateLimiter,
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    format: MediaFormat,
) -> Result<Ended> {
    let cfg = &inner.config;
    let limit = rate(&inner.speed_limit, limiter);
    let connections = record
        .connections
        .unwrap_or_else(|| inner.connections())
        .clamp(1, MAX_FRAGMENT_CONNECTIONS);
    let mut cmd = tools::ytdlp_command(ytdlp, cfg.tools_dir.as_deref());
    cmd.args(args(record, format, ffmpeg, limit, connections))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut tree = Tree::spawn(&mut cmd).map_err(|e| Error::Media {
        message: format!("could not start yt-dlp: {e}"),
        retryable: false,
    })?;
    let (Some(stdout), Some(stderr)) = (tree.child().stdout.take(), tree.child().stderr.take())
    else {
        unreachable!("both are piped");
    };
    let mut out = BufReader::new(stdout).split(b'\n');
    let mut err = BufReader::new(stderr).split(b'\n');
    let (mut out_open, mut err_open) = (true, true);
    let mut errors = Vec::new();

    let mut tracker = Tracker { estimate: record.total, ..Tracker::default() };
    let mut ticker = time::interval(cfg.progress_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_checkpoint = Instant::now();
    let mut last_data = Instant::now();
    while out_open || err_open {
        tokio::select! {
            () = cancel.cancelled() => {
                tree.kill().await;
                save_progress(record, &tracker);
                return Ok(Ended::Stopped);
            }
            line = out.next_segment(), if out_open => match line {
                Ok(Some(line)) => {
                    if tracker.line(&String::from_utf8_lossy(&line)) {
                        last_data = Instant::now();
                    }
                }
                _ => out_open = false,
            },
            line = err.next_segment(), if err_open => match line {
                Ok(Some(line)) => {
                    let line = String::from_utf8_lossy(&line).trim_end().to_string();
                    tracing::debug!(id = %record.id, "yt-dlp: {line}");
                    if line.starts_with("ERROR:") {
                        errors.push(line);
                    }
                }
                _ => err_open = false,
            },
            _ = ticker.tick() => {
                let now = Instant::now();
                inner.emit(Event::Progress(Progress {
                    id: record.id,
                    downloaded: tracker.downloaded(),
                    total: tracker.total(),
                    speed: tracker.speed,
                    connections: usize::from(tracker.receiving),
                    stalled: tracker.receiving && now.duration_since(last_data) >= cfg.stall_after,
                    problem: None,
                    segments: vec![tracker.segment()],
                }));
                if now.duration_since(last_checkpoint) >= cfg.checkpoint_interval {
                    last_checkpoint = now;
                    save_progress(record, &tracker);
                    checkpoint(inner, record).await;
                }
                if rate(&inner.speed_limit, limiter) != limit {
                    tree.kill().await;
                    return Ok(Ended::LimitChanged);
                }
            }
        }
    }
    let status = tree.child().wait().await?;
    save_progress(record, &tracker);
    if !status.success() {
        return Err(match failure(errors.iter().map(String::as_str), status.code()) {
            Error::Media { message, .. }
                if message.starts_with("Requested format is not available") =>
            {
                Error::Media { message: missing_format(ffmpeg.is_some()).into(), retryable: false }
            }
            e => e,
        });
    }
    let Some(file) = tracker.file.as_deref().and_then(Path::file_name) else {
        return Err(Error::Media {
            message: "yt-dlp finished without saying where it saved the file".into(),
            retryable: false,
        });
    };
    record.file_name = file.to_string_lossy().into_owned();
    if let Ok(meta) = std::fs::metadata(record.path()) {
        record.total = Some(meta.len());
        record.segments = vec![Segment { start: 0, end: meta.len(), downloaded: meta.len() }];
    }
    inner.save_running(record)?;
    Ok(Ended::Finished)
}

fn save_progress(record: &mut DownloadRecord, tracker: &Tracker) {
    record.total = tracker.total();
    record.segments = vec![tracker.segment()];
}

async fn checkpoint(inner: &Arc<Inner>, record: &DownloadRecord) {
    let (inner, mut record) = (inner.clone(), record.clone());
    let id = record.id;
    match tokio::task::spawn_blocking(move || inner.save_running(&mut record)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(%id, "checkpoint failed: {e}"),
        Err(e) => tracing::warn!(%id, "checkpoint task failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn youtube_like() -> Value {
        json!({
            "title": "Me at the zoo",
            "formats": [
                {"format_id": "sb0", "ext": "mhtml", "vcodec": "none", "acodec": "none"},
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "filesize": 300, "tbr": 129},
                {"format_id": "251", "ext": "webm", "vcodec": "none", "acodec": "opus", "filesize": 250, "tbr": 140},
                {"format_id": "160", "ext": "mp4", "vcodec": "avc1", "acodec": "none", "height": 144, "fps": 15, "filesize": 100, "tbr": 50},
                {"format_id": "278", "ext": "webm", "vcodec": "vp9", "acodec": "none", "height": 144, "fps": 15, "filesize": 90, "tbr": 60},
                {"format_id": "399", "ext": "mp4", "vcodec": "av01", "acodec": "none", "height": 1080, "fps": 60, "filesize_approx": 5000, "tbr": 3000},
                {"format_id": "248", "ext": "webm", "vcodec": "vp9", "acodec": "none", "height": 1080, "fps": 60, "tbr": 4000},
            ]
        })
    }

    #[test]
    fn offers_a_choice_per_height_and_the_sound() {
        let choices = choices(&youtube_like(), true);
        let labels: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["1080p60", "144p", "Audio only"]);
        assert_eq!(choices[0].ext, "mp4");
        assert_eq!(choices[0].size, Some(5300));
        assert_eq!(choices[0].format, MediaFormat::Video { max_height: Some(1080) });
        assert_eq!(choices[2].format, MediaFormat::Audio);
        assert_eq!(choices[2].ext, "m4a");
    }

    #[test]
    fn without_ffmpeg_offers_only_formats_with_sound() {
        let mut info = youtube_like();
        info["formats"].as_array_mut().unwrap().push(json!(
            {"format_id": "18", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a", "height": 360, "filesize": 700}
        ));
        let labels: Vec<_> = choices(&info, false).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, ["360p", "Audio only"]);
    }

    #[test]
    fn a_single_file_page_offers_the_best_quality() {
        let info = json!({"title": "clip", "ext": "mp4", "url": "https://x/clip.mp4"});
        let choices = choices(&info, false);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].format, MediaFormat::Video { max_height: None });
        assert_eq!(choices[0].ext, "mp4");
    }

    #[test]
    fn follows_progress_across_files() {
        let mut t = Tracker::default();
        t.line("BLAZER-SIZE 1000\r");
        assert_eq!(t.total(), Some(1000));
        assert!(t.line("BLAZER-PROGRESS 137 downloading 100 700 NA 5000.5"));
        assert_eq!((t.downloaded(), t.speed), (100, 5000));
        t.line("BLAZER-PROGRESS 137 finished 700 700 NA NA");
        assert!(!t.receiving);
        t.line("BLAZER-PROGRESS 140 downloading 50 NA 300.0 NA");
        assert_eq!(t.downloaded(), 750);
        // A file finished in an earlier run reports only its size.
        let mut t = Tracker::default();
        t.line("BLAZER-PROGRESS 137 finished NA 700 NA NA");
        assert_eq!(t.downloaded(), 700);
        t.line("BLAZER-FILE C:\\Videos\\clip.mp4");
        assert_eq!(t.file.as_deref(), Some(Path::new("C:\\Videos\\clip.mp4")));
    }

    #[test]
    fn explains_failures() {
        let error = failure(
            ["WARNING: x", "ERROR: [youtube] aaaaaaaaaaa: This video is unavailable"].into_iter(),
            Some(1),
        );
        assert!(
            matches!(&error, Error::Media { message, retryable: false } if message == "This video is unavailable")
        );
        let error = failure(
            ["ERROR: [generic] Unable to download webpage: HTTP Error 404: Not Found"].into_iter(),
            Some(1),
        );
        assert!(
            matches!(&error, Error::Media { message, retryable: false } if message.starts_with("Unable to download webpage"))
        );
        let error = failure(
            ["ERROR: Unable to download webpage: <urlopen error timed out>"].into_iter(),
            Some(1),
        );
        assert!(error.is_retryable());
        assert!(failure([].into_iter(), Some(3)).is_retryable());
    }

    #[test]
    fn recognizes_its_working_files() {
        for rest in [
            "mp4.part",
            "f137.mp4",
            "f140-drc.m4a.part",
            "f137.mp4.part-Frag12",
            "mp4.ytdl",
            "temp.mp4",
            "fhls-1080p.mp4",
        ] {
            assert!(is_working_file(rest), "{rest}");
        }
        for rest in ["mp4", "final.mp4", "mkv", "partial.txt"] {
            assert!(!is_working_file(rest), "{rest}");
        }
    }

    #[test]
    fn escapes_names_for_the_output_template() {
        assert_eq!(escape_template("100% fun"), "100%% fun");
        assert_eq!(stem("a.b.mp4"), "a.b");
        assert_eq!(stem("noext"), "noext");
    }
}
