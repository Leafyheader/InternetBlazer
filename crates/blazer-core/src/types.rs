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

/// An automatic retry: the how-manyth in a row, and when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retry {
    /// Counts from 1, and starts over whenever an attempt makes progress.
    pub attempt: u32,
    /// Unix timestamp in seconds.
    pub at: u64,
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
    /// Nothing has arrived for a few seconds, and not because of a speed limit.
    pub stalled: bool,
    /// While stalled: the latest reason a connection failed, such as the network being down.
    /// Connections keep trying again on their own for a while before the download itself
    /// counts as failed and turns [`Status::Retrying`].
    pub problem: Option<String>,
    /// Sorted by start offset.
    pub segments: Vec<Segment>,
}

/// What a link points to, found out without downloading it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkInfo {
    /// The name the server suggests, before it is made unique in a folder.
    pub file_name: Option<String>,
    pub total: Option<u64>,
    pub resumable: bool,
}

/// Engine-wide settings that persist in the [`Store`](crate::Store).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Bytes per second across all downloads.
    pub speed_limit: Option<u64>,
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
    /// A queue was added, changed or removed.
    QueuesChanged,
    SettingsChanged(Settings),
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
