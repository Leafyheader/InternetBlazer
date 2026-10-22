//! Download engine for InternetBlazer.
//!
//! Files are split into segments that are fetched over parallel HTTP range requests. Whenever a
//! connection runs out of work it splits the largest remaining segment in half, so every
//! connection stays busy until the very end. Progress is checkpointed to a [`Store`], which lets
//! downloads resume after a pause, a crash or a power loss.
//!
//! Started downloads wait in a [`Queue`] that runs a limited number at a time, and can be
//! limited in speed, each and all together.

mod download;
mod engine;
mod error;
mod filename;
mod fsio;
mod limit;
mod probe;
mod queue;
mod segment;
mod store;
mod types;

pub use engine::{Config, Engine, MAX_CONNECTIONS, NewDownload};
pub use error::{Error, Result};
pub use limit::MIN_SPEED_LIMIT;
pub use queue::{ALL_DAYS, Queue, Schedule};
pub use segment::Segment;
pub use store::{RedbStore, Store};
pub use types::{DownloadId, DownloadRecord, Event, Progress, QueueId, Settings, Status};
