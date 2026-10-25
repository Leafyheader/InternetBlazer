use std::{
    fmt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{filename, segment::Segment};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DownloadId(pub u64);

impl fmt::Display for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct QueueId(pub u64);

impl QueueId {
    /// The queue that always exists and that downloads join by default.
    pub const MAIN: Self = Self(0);
}

impl fmt::Display for QueueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Waiting for a free slot in its queue.
    Queued,
    Downloading,
    /// Waiting to try again after a temporary failure, such as a lost connection. See
    /// [`DownloadRecord::retry`].
    Retrying,
    Paused,
    Completed,
    Failed,
}

/// Everything the engine persists about a download.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRecord {
    pub id: DownloadId,
    pub url: String,
    /// Extra request headers, such as cookies or a referrer passed on by the browser.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub dir: PathBuf,
    pub file_name: String,
    /// `None` when the server did not report a size.
    pub total: Option<u64>,
    /// Whether the server supports byte ranges, which multi-connection and resume rely on.
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Overrides [`Config::connections`](crate::Config::connections) for this download.
    pub connections: Option<usize>,
    #[serde(default)]
    pub queue: QueueId,
    /// Bytes per second, applied on top of the global limit.
    #[serde(default)]
    pub speed_limit: Option<u64>,
    /// Set for a video or audio on a web page, which yt-dlp downloads instead of the engine,
    /// unless it is a [`stream`](Self::stream). `file_name` is then a guess until it finishes,
    /// since the final extension depends on what the site sends.
    #[serde(default)]
    pub media: Option<MediaFormat>,
    /// For a media download: `url` is an HLS or DASH playlist, whose pieces the engine downloads
    /// itself instead of yt-dlp.
    #[serde(default)]
    pub stream: bool,
    /// For such a stream, how far each of its tracks got.
    #[serde(default)]
    pub tracks: Vec<TrackProgress>,
    /// For a media download, a single segment that counts its bytes.
    pub segments: Vec<Segment>,
    pub status: Status,
    pub error: Option<String>,
    /// The automatic retry after a temporary failure, kept while the download tries again.
    #[serde(default)]
    pub retry: Option<Retry>,
    /// It was running or waiting to run when the engine last stopped without anyone pausing it,
    /// as when the app quits or crashes. [`Engine::resume_interrupted`](crate::Engine::resume_interrupted) picks
    /// these up again.
    #[serde(default)]
    pub interrupted: bool,
    /// Unix timestamp in seconds.
    pub added_at: u64,
    pub completed_at: Option<u64>,
    /// When it last started downloading. `None` for downloads never started, and for those
    /// added before this was kept.
    #[serde(default)]
    pub last_try_at: Option<u64>,
    /// The SHA-256 the file should have, checked once it finishes.
    #[serde(default)]
    pub checksum: Option<Checksum>,
}

impl DownloadRecord {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }

    /// Where data is written until the download completes.
    pub fn part_path(&self) -> PathBuf {
        filename::part_path(&self.path())
    }

    pub fn downloaded(&self) -> u64 {
        self.segments.iter().map(|s| s.downloaded).sum()
    }
}

/// How much of one track of an HLS or DASH stream, such as its picture or its sound, is in its
/// file. Pieces are written in order, so a paused download goes on from the next one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackProgress {
    /// Pieces written, from the first.
    pub pieces: usize,
    /// Pieces the track has, which tells whether a playlist read again is still the same.
    pub total_pieces: usize,
    /// The length of the track's file.
    pub bytes: u64,
}

impl TrackProgress {
    pub fn is_complete(&self) -> bool {
        self.pieces == self.total_pieces
    }
}

/// An automatic retry: the how-manyth in a row, and when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retry {
    /// Counts from 1, and starts over whenever an attempt makes progress.
    pub attempt: u32,
    /// Unix timestamp in seconds.
    pub at: u64,
}

/// A SHA-256 the downloaded file should have, such as one published next to the link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checksum {
    /// In lowercase hex.
    pub expected: String,
    pub state: CheckState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CheckState {
    /// Not checked yet: the download hasn't finished, or the check was cut off.
    Waiting,
    Checking,
    Matched,
    /// The file is damaged, or isn't the one the hash belongs to.
    Mismatched { actual: String },
    /// The file couldn't be read, such as after it was moved or deleted outside the app.
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub id: DownloadId,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Smoothed speed in bytes per second.
    pub speed: u64,
    /// Connections currently receiving data.
    pub connections: usize,
    /// Connections the download set out to use: the setting, or 1 when the server can't send
    /// the file in parts.
    pub planned: usize,
    /// Connections given up because the server asked for fewer.
    pub refused: usize,
    /// Connections that found no part left worth splitting off, near the end or for a small file.
    pub idle: usize,
    /// Nothing has arrived for a few seconds, and not because of a speed limit.
    pub stalled: bool,
    /// While stalled: the latest reason a connection failed, such as the network being down.
    /// Connections keep trying again on their own for a while before the download itself
    /// counts as failed and turns [`Status::Retrying`].
    pub problem: Option<String>,
    /// Sorted by start offset.
    pub segments: Vec<Segment>,
    /// While the downloaded parts of a video are put together into the finished file, how far
    /// that got, from 0 to 1.
    pub joining: Option<f64>,
}

impl Progress {
    /// Joining `bytes` of downloaded parts into the finished file, `fraction` of the way.
    pub(crate) fn joining(id: DownloadId, bytes: u64, fraction: f64) -> Self {
        Self {
            id,
            downloaded: bytes,
            total: Some(bytes),
            speed: 0,
            connections: 0,
            planned: 1,
            refused: 0,
            idle: 0,
            stalled: false,
            problem: None,
            segments: vec![Segment { start: 0, end: bytes, downloaded: bytes }],
            joining: Some(fraction),
        }
    }
}

/// What a link points to, found out without downloading it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkInfo {
    /// The name the server suggests, before it is made unique in a folder.
    pub file_name: Option<String>,
    pub total: Option<u64>,
    pub resumable: bool,
    /// The link leads to a web page rather than a file, such as a page with a video on it.
    pub page: bool,
}

/// What a media download fetches: the best video up to a height, or only the sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MediaFormat {
    #[serde(rename_all = "camelCase")]
    Video {
        /// In pixels; `None` takes the best there is.
        max_height: Option<u32>,
    },
    Audio,
}

/// The videos or sounds yt-dlp found on a page.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub title: String,
    /// Such as `Youtube`.
    pub site: String,
    /// In seconds.
    pub duration: Option<f64>,
    /// Best first, then the audio-only choice if there is one.
    pub choices: Vec<MediaChoice>,
    /// Whether ffmpeg was found. Without it, sites that keep video and sound apart (as YouTube
    /// does) offer only the qualities that come in one file, which may be none.
    pub ffmpeg: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaChoice {
    /// Such as `1080p60` or `Audio only`.
    pub label: String,
    pub format: MediaFormat,
    /// The likely extension of the file, without the dot.
    pub ext: String,
    /// An estimate; sites don't always say.
    pub size: Option<u64>,
}

/// The programs media downloads use.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaTools {
    /// `None` until it is installed.
    pub ytdlp: Option<Tool>,
    /// Runs YouTube's JavaScript for yt-dlp. Without it, YouTube offers yt-dlp fewer formats.
    pub deno: Option<Tool>,
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub path: PathBuf,
    pub version: String,
    /// The engine installed this copy and can update it; otherwise it was found on `PATH`.
    pub managed: bool,
}

/// Engine-wide settings that persist in the [`Store`](crate::Store).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Bytes per second across all downloads.
    pub speed_limit: Option<u64>,
    /// Connections for downloads that don't set their own. `None` uses the engine's
    /// [`Config`](crate::Config).
    pub connections: Option<usize>,
    /// Automatic retries of a download that failed for a temporary reason. `None` uses the
    /// engine's [`Config`](crate::Config).
    pub auto_retries: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    Added(DownloadRecord),
    Progress(Progress),
    StatusChanged {
        id: DownloadId,
        status: Status,
        error: Option<String>,
    },
    /// A setting of the download changed, such as its queue or speed limit. Its progress fields
    /// may lag behind the latest [`Event::Progress`] while it runs.
    Updated(DownloadRecord),
    Removed {
        id: DownloadId,
    },
    /// The expected SHA-256 was set or cleared, or a check of it started or ended.
    ChecksumChanged {
        id: DownloadId,
        checksum: Option<Checksum>,
    },
    /// A queue was added, changed or removed.
    QueuesChanged,
    SettingsChanged(Settings),
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
