//! Persistence of download records, queues and settings.

use std::{fs, path::Path};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    error::{Error, Result},
    queue::Queue,
    types::{DownloadId, DownloadRecord, QueueId, Settings},
};

/// Where the engine keeps its state. Implementations must be crash-safe: after a successful
/// save, the data must survive a power loss.
pub trait Store: Send + Sync + 'static {
    fn next_id(&self) -> Result<DownloadId>;
    fn save(&self, record: &DownloadRecord) -> Result<()>;
    fn get(&self, id: DownloadId) -> Result<Option<DownloadRecord>>;
    /// All records, oldest first.
    fn list(&self) -> Result<Vec<DownloadRecord>>;
    fn delete(&self, id: DownloadId) -> Result<()>;

    /// All queues, by ID.
    fn queues(&self) -> Result<Vec<Queue>>;
    fn save_queue(&self, queue: &Queue) -> Result<()>;
    fn delete_queue(&self, id: QueueId) -> Result<()>;

    /// The defaults when nothing was saved yet.
    fn settings(&self) -> Result<Settings>;
    fn save_settings(&self, settings: &Settings) -> Result<()>;
}

/// Values are stored as JSON so new fields can be added without migrations.
const DOWNLOADS: TableDefinition<u64, &[u8]> = TableDefinition::new("downloads");
const QUEUES: TableDefinition<u64, &[u8]> = TableDefinition::new("queues");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const CONFIG: TableDefinition<&str, &[u8]> = TableDefinition::new("config");
const NEXT_ID: &str = "next_id";
const SETTINGS: &str = "settings";

type JsonTable = TableDefinition<'static, u64, &'static [u8]>;

pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let db = Database::create(path).map_err(db_err)?;
        let tx = db.begin_write().map_err(db_err)?;
        tx.open_table(DOWNLOADS).map_err(db_err)?;
        tx.open_table(QUEUES).map_err(db_err)?;
        tx.open_table(META).map_err(db_err)?;
        tx.open_table(CONFIG).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(Self { db })
    }

    fn put(&self, table: JsonTable, key: u64, value: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec(value).map_err(db_err)?;
        let tx = self.db.begin_write().map_err(db_err)?;
        tx.open_table(table).map_err(db_err)?.insert(key, bytes.as_slice()).map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    fn get_one<T: DeserializeOwned>(&self, table: JsonTable, key: u64) -> Result<Option<T>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(table).map_err(db_err)?;
        let Some(value) = table.get(key).map_err(db_err)? else { return Ok(None) };
        serde_json::from_slice(value.value()).map(Some).map_err(db_err)
    }

    fn get_all<T: DeserializeOwned>(&self, table: JsonTable) -> Result<Vec<T>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(table).map_err(db_err)?;
        let mut values = Vec::new();
        for entry in table.iter().map_err(db_err)? {
            let (_, value) = entry.map_err(db_err)?;
            values.push(serde_json::from_slice(value.value()).map_err(db_err)?);
        }
        Ok(values)
    }

    fn remove(&self, table: JsonTable, key: u64) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        tx.open_table(table).map_err(db_err)?.remove(key).map_err(db_err)?;
        tx.commit().map_err(db_err)
    }
}

impl Store for RedbStore {
    fn next_id(&self) -> Result<DownloadId> {
        let tx = self.db.begin_write().map_err(db_err)?;
        let id = {
            let mut meta = tx.open_table(META).map_err(db_err)?;
            let id = meta.get(NEXT_ID).map_err(db_err)?.map_or(1, |v| v.value());
            meta.insert(NEXT_ID, id + 1).map_err(db_err)?;
            id
        };
        tx.commit().map_err(db_err)?;
        Ok(DownloadId(id))
    }

    fn save(&self, record: &DownloadRecord) -> Result<()> {
        self.put(DOWNLOADS, record.id.0, record)
    }

    fn get(&self, id: DownloadId) -> Result<Option<DownloadRecord>> {
        self.get_one(DOWNLOADS, id.0)
    }

    fn list(&self) -> Result<Vec<DownloadRecord>> {
        self.get_all(DOWNLOADS)
    }

    fn delete(&self, id: DownloadId) -> Result<()> {
        self.remove(DOWNLOADS, id.0)
    }

    fn queues(&self) -> Result<Vec<Queue>> {
        self.get_all(QUEUES)
    }

    fn save_queue(&self, queue: &Queue) -> Result<()> {
        self.put(QUEUES, queue.id.0, queue)
    }

    fn delete_queue(&self, id: QueueId) -> Result<()> {
        self.remove(QUEUES, id.0)
    }

    fn settings(&self) -> Result<Settings> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let table = tx.open_table(CONFIG).map_err(db_err)?;
        let Some(value) = table.get(SETTINGS).map_err(db_err)? else {
            return Ok(Settings::default());
        };
        serde_json::from_slice(value.value()).map_err(db_err)
    }

    fn save_settings(&self, settings: &Settings) -> Result<()> {
        let bytes = serde_json::to_vec(settings).map_err(db_err)?;
        let tx = self.db.begin_write().map_err(db_err)?;
        tx.open_table(CONFIG)
            .map_err(db_err)?
            .insert(SETTINGS, bytes.as_slice())
            .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }
}

fn db_err(e: impl std::fmt::Display) -> Error {
    Error::Store(e.to_string())
}
