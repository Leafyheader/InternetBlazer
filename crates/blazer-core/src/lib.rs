//! Download engine for InternetBlazer.
//!
//! Files are split into segments that are fetched over parallel HTTP range requests. Whenever a
//! connection runs out of work it splits the largest remaining segment in half, so every
//! connection stays busy until the very end. Progress is checkpointed to a [`Store`], which lets
//! downloads resume after a pause, a crash or a power loss. A download that fails for a
//! temporary reason, such as a lost connection, tries again by itself after a growing pause.
//!
//! Started downloads wait in a [`Queue`] that runs a limited number at a time, and can be
//! limited in speed, each and all together.
//!
//! Videos and sounds on web pages are downloaded by yt-dlp instead, which the engine runs, can
//! install, and schedules like any other download. An HLS or DASH playlist that a video player
//! fetched is the exception: the engine downloads its pieces itself, as IDM does.

mod checksum;
mod child;
mod download;
mod engine;
mod error;
mod filename;
mod fsio;
mod limit;
mod media;
mod playlist;
mod probe;
mod queue;
mod segment;
mod sniff;
mod store;
mod stream;
mod tools;
mod types;

pub use checksum::parse_sha256;
pub use engine::{Config, Engine, MAX_AUTO_RETRIES, MAX_CONNECTIONS, NewDownload};
pub use error::{Error, Result};
pub use limit::MIN_SPEED_LIMIT;
pub use queue::{ALL_DAYS, Queue, Schedule};
pub use segment::Segment;
pub use store::{RedbStore, Store};
pub use types::{
    CheckState, Checksum, DownloadId, DownloadRecord, Event, LinkInfo, MediaChoice, MediaFormat,
    MediaInfo, MediaTools, Progress, QueueId, Retry, Settings, Status, Tool, TrackProgress,
};

/// Bytes free for this user on the disk that would hold `path`, which need not exist yet.
pub fn free_space(path: &std::path::Path) -> std::io::Result<u64> {
    fsio::available_space(path)
}
