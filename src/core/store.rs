use crate::core::error::{Error, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const SCHEMA: &str = include_str!("schema.sql");

/// Owns the connection and the schema. One global DB; projects are rows, not files.
pub struct Store {
    pub conn: Connection,
}

impl Store {
    /// Where the single global DB lives.
    ///
    /// `AI_KANBAN_DB` overrides everything. That override is not a convenience -- without
    /// it, every test and every dev run writes into the developer's real memory store.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(p) = std::env::var_os("AI_KANBAN_DB") {
            return Ok(PathBuf::from(p));
        }
        let dir = dirs::data_dir()
            .ok_or_else(|| Error::Other("no platform data directory on this system".into()))?
            .join("ai-kanban");
        Ok(dir.join("kanban.db"))
    }

    /// Opens the global store, creating it on first write. There is no init step -- an
    /// init step is setup overhead, and "little setup overhead" is the whole pitch.
    pub fn open_default() -> Result<Self> {
        Self::open(&Self::default_path()?)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL lets sessions in different projects write concurrently with no daemon.
        // In-memory DBs reject it; that is fine and not worth failing over.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Wait rather than fail when another session holds the write lock. Multiple
        // concurrent agent sessions are the normal case, not the exception.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }
}

/// Unix epoch seconds. Every timestamp in the store is this.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    /// Opens the store only if it already exists, never creating it.
    ///
    /// The counterpart to `find_project`: a read-only consumer such as the session-start
    /// hook must not bring a database into being merely by running. Returns `None` when
    /// there is nothing to read, so the caller can stay silent instead of reporting an
    /// error for the entirely normal case of "this user has not used ai-kanban yet".
    pub fn open_existing() -> Result<Option<Self>> {
        let path = Self::default_path()?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Self::open(&path)?))
    }
}
