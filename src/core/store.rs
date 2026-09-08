use crate::core::error::{Error, Result};
use crate::core::migrate;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Owns the connection and the schema. One global DB; projects are rows, not files.
pub struct Store {
    pub conn: Connection,
    /// Set only by `open_existing`. The checkpoint in `Drop` cannot run on a read-only
    /// connection, and attempting it there looked like it did something while it could not.
    read_only: bool,
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
        // Creates the schema on a new store and upgrades an old one. This is the only place
        // that happens -- see `migrate.rs` for why the read-only path must not.
        migrate::run(&conn)?;
        Ok(Self { conn, read_only: false })
    }
}

impl Store {
    /// This store's schema version. `docs/http-api.md` has `/api/meta` reporting it, and it
    /// is the first thing worth knowing about a store that is behaving oddly.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }
}

/// Fold the WAL back into the main file when the last handle goes away.
///
/// This does not make `cp kanban.db` correct -- `backup_to` is the correct way, and the
/// WAL can still be non-empty here because another session holds the lock. It makes the
/// *usual* case safe: after a clean exit the main file is current, so the naive copy that
/// people and backup tools take anyway is usually not a silent four-day rollback (task
/// #14, where exactly that happened on the real store).
///
/// Failure is ignored on purpose. Every caller of this is a process already on its way
/// out, and a busy checkpoint means another session will do the same job shortly -- neither
/// is worth a message in a destructor.
///
/// The read-only connection skips it rather than attempting and swallowing the failure. A
/// call that cannot possibly work reads, to anyone maintaining this, as one that does.
impl Drop for Store {
    fn drop(&mut self) {
        if self.read_only {
            return;
        }
        self.checkpoint();
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
    /// Opens the store **read-only**, and only if it already exists.
    ///
    /// The counterpart to `find_project`, for consumers like the session-start hook that
    /// must observe without touching anything. Returns `None` when there is nothing to
    /// read, so the caller can stay silent rather than report an error for the entirely
    /// normal case of "this user has not used ai-kanban yet".
    ///
    /// Two deliberate differences from `open`:
    ///
    /// * `SQLITE_OPEN_READ_ONLY`, so a write is impossible rather than merely unintended.
    /// * Migrations are **not** run. They would execute at session start, in every directory
    ///   the user opens Claude Code in, under the host's hook timeout -- and they write.
    ///   Migrations belong in the paths that already intend to write. The cost is that this
    ///   path can meet a store older than its own queries, which `migrate::is_readable`
    ///   turns into silence.
    pub fn open_existing() -> Result<Option<Self>> {
        let path = Self::default_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Still worth waiting briefly: a reader can block behind a checkpoint. Kept well
        // under the hook timeout so a stall ends in silence rather than a killed process.
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        // A store older than this binary's read queries is reported as "nothing to read",
        // so the caller stays silent instead of running a query against a column that is
        // not there yet. It cannot be fixed here: this path must never write.
        if !migrate::is_readable(&conn)? {
            return Ok(None);
        }
        Ok(Some(Self { conn, read_only: true }))
    }
}
