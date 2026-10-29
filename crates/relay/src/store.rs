//! The relay's own database. Never the kanban store.
//!
//! # Why a separate schema
//!
//! The server stores who may write and an ordered log of changes it does not read. Putting
//! that into core's schema would tie the relay to every core migration, and put empty server
//! tables into every personal store for no reader. It has its own `user_version` chain and its
//! own `application_id`, and it refuses a file that carries neither -- so pointing `--db` at a
//! personal `kanban.db` fails at startup instead of writing server tables into it.

use ai_kanban_protocol::{self as protocol, Change, HighWater, Joined, Member, Page, Pulled, Pushed, Ref, RoomInfo};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Unix epoch seconds, as everywhere in ai-kanban.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
use std::path::{Path, PathBuf};

/// Marks a file as a relay database. Arbitrary, fixed forever; `PRAGMA application_id` exists
/// for exactly this. ("AKbS" in ASCII.)
const APPLICATION_ID: i64 = 0x414B_6253;

/// Each entry runs once, in order, inside a transaction, and stamps `user_version`. Frozen once
/// shipped, for the reason note #178 gives about core's migrations: a store that already ran
/// one never runs an edited version of it.
const MIGRATIONS: &[&str] = &[include_str!("schema.sql"), include_str!("migrate_002_room_board.sql")];

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("missing or unknown key")]
    Unauthorized,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    /// The request contradicts what the server already holds -- a reused change id, a tag
    /// someone else owns.
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ServerError>;

/// Who a request is from, resolved from its key.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user: String,
    pub tag: String,
    pub key_id: i64,
}

#[derive(Debug, Clone)]
pub struct KeyRow {
    pub id: i64,
    pub user: String,
    pub tag: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub name: String,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
    pub tags: Vec<String>,
}

/// What `forget` and `delete_room` would remove, so the CLI can show it before `--yes`.
#[derive(Debug, Clone, Default)]
pub struct Removal {
    pub changes: i64,
    pub members: i64,
}

pub struct ServerDb {
    pub conn: Connection,
    path: Option<PathBuf>,
}

impl ServerDb {
    /// Where the relay keeps its database unless told otherwise. `/data` is the container's
    /// volume. Deliberately no fallback to `AI_KANBAN_DB` or the platform data directory: a
    /// server started on a laptop must never open the personal store.
    pub fn default_path() -> PathBuf {
        std::env::var_os("AI_KANBAN_SERVER_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/data/server.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let app: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table'", [], |r| r.get(0))?;
        if app != APPLICATION_ID && tables > 0 {
            return Err(ServerError::Other(
                "this file is not an ai-kanban-relay database (is it a personal kanban.db?). \
                 Point --db or AI_KANBAN_SERVER_DB at a new file."
                    .into(),
            ));
        }
        let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version as usize > MIGRATIONS.len() {
            return Err(ServerError::Other(format!(
                "server database is version {version}, newer than this binary ({}). Upgrade the image.",
                MIGRATIONS.len()
            )));
        }
        while (version as usize) < MIGRATIONS.len() {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATIONS[version as usize])?;
            version += 1;
            tx.pragma_update(None, "user_version", version)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        }
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Users, keys, tags
// ---------------------------------------------------------------------------

/// 32 random bytes as hex, prefixed so a key pasted into the wrong place is recognisable.
fn new_key() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| ServerError::Other(format!("no randomness: {e}")))?;
    Ok(format!("akb_{}", hex(&buf)))
}

/// SHA-256, not a slow password hash: a key is 256 random bits, so there is nothing for
/// argon2 to protect against, and every request would pay for it.
fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(key.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl ServerDb {
    pub fn add_user(&self, name: &str) -> Result<()> {
        let name = name.trim();
        // Same spelling rule as a room: it is typed on command lines and printed next to tags.
        if protocol::room_name(name).as_deref() != Some(name) {
            return Err(ServerError::BadRequest(format!(
                "user name {name:?}: lowercase letters, digits, '-' and '_' only"
            )));
        }
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO users (name, created_at) VALUES (?1, ?2)",
            params![name, now()],
        )?;
        if n == 0 {
            return Err(ServerError::Conflict(format!("user {name} exists")));
        }
        Ok(())
    }

    pub fn users(&self) -> Result<Vec<UserRow>> {
        let mut st = self.conn.prepare(
            "SELECT u.name, u.created_at, u.disabled_at,
                    COALESCE((SELECT group_concat(tag, ',') FROM (SELECT tag FROM tags WHERE user_id = u.id ORDER BY tag)), '')
               FROM users u ORDER BY u.name",
        )?;
        let rows = st.query_map([], |r| {
            let tags: String = r.get(3)?;
            Ok(UserRow {
                name: r.get(0)?,
                created_at: r.get(1)?,
                disabled_at: r.get(2)?,
                tags: tags.split(',').filter(|t| !t.is_empty()).map(String::from).collect(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn disable_user(&self, name: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE users SET disabled_at = ?2 WHERE name = ?1 AND disabled_at IS NULL",
            params![name, now()],
        )?;
        if n == 0 {
            return Err(ServerError::NotFound(format!("no active user {name}")));
        }
        Ok(())
    }

    fn user_id(&self, name: &str) -> Result<i64> {
        self.conn
            .query_row("SELECT id FROM users WHERE name = ?1", [name], |r| r.get(0))
            .optional()?
            .ok_or_else(|| ServerError::NotFound(format!("no user {name}")))
    }

    /// A key for one more of `user`'s machines, under the next free tag: their initial and the
    /// lowest unused number (`volker` gets `v1`, then `v2` for a second machine). Returns the
    /// tag and the key.
    ///
    /// Picked rather than asked for: the admin had to invent a tag per key and gained nothing by
    /// it. One tag per machine stays, because a store's task numbers are its own -- two machines
    /// under one tag would both publish `#1-v1`.
    pub fn issue_key(&self, user: &str) -> Result<(String, String)> {
        self.user_id(user)?;
        let initial = user.chars().next().filter(char::is_ascii_lowercase).unwrap_or('u');
        let mut n = 1;
        let tag = loop {
            let tag = format!("{initial}{n}");
            let taken: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM tags WHERE tag = ?1)", [&tag], |r| r.get(0))?;
            if !taken {
                break tag;
            }
            n += 1;
        };
        let key = self.add_key(user, &tag)?;
        Ok((tag, key))
    }

    /// The tags `user` holds, oldest first: one per machine.
    pub fn tags_of(&self, user: &str) -> Result<Vec<String>> {
        let uid = self.user_id(user)?;
        let mut st = self.conn.prepare("SELECT tag FROM tags WHERE user_id = ?1 ORDER BY created_at, tag")?;
        let tags = st.query_map([uid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(tags)
    }

    /// Issues a key under a new origin tag and returns it. The plain key exists only in this
    /// return value; the database keeps its hash.
    ///
    /// A tag belongs to one user forever, even after its keys are revoked: tasks published as
    /// `#122-v1` keep that handle, and handing `v1` to someone else later would make their new
    /// tasks collide with it.
    pub fn add_key(&self, user: &str, tag: &str) -> Result<String> {
        let uid = self.user_id(user)?;
        if !protocol::valid_tag(tag) {
            return Err(ServerError::BadRequest(format!(
                "tag {tag:?}: a lowercase letter, then up to 7 lowercase letters or digits (no '-')"
            )));
        }
        if tag == protocol::SERVER_TAG {
            return Err(ServerError::BadRequest(format!("tag {tag:?} is reserved for the server")));
        }
        let tx = self.conn.unchecked_transaction()?;
        let owner: Option<String> = tx
            .query_row("SELECT u.name FROM tags t JOIN users u ON u.id = t.user_id WHERE t.tag = ?1", [tag], |r| r.get(0))
            .optional()?;
        if let Some(owner) = owner {
            return Err(ServerError::Conflict(format!(
                "tag {tag} already belongs to {owner}. Tags are never reused; use `key rotate {tag}` for a new key under it"
            )));
        }
        tx.execute("INSERT INTO tags (tag, user_id, created_at) VALUES (?1, ?2, ?3)", params![tag, uid, now()])?;
        let key = new_key()?;
        tx.execute("INSERT INTO keys (tag, hash, created_at) VALUES (?1, ?2, ?3)", params![tag, hash_key(&key), now()])?;
        tx.commit()?;
        Ok(key)
    }

    /// A new key for an existing tag, revoking the old one. For a leaked or lost key: the
    /// machine keeps its tag, so everything it published keeps its handles.
    pub fn rotate_key(&self, tag: &str) -> Result<String> {
        let tx = self.conn.unchecked_transaction()?;
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tags WHERE tag = ?1)", [tag], |r| r.get(0))?;
        if !exists {
            return Err(ServerError::NotFound(format!("no tag {tag}")));
        }
        tx.execute("UPDATE keys SET revoked_at = ?2 WHERE tag = ?1 AND revoked_at IS NULL", params![tag, now()])?;
        let key = new_key()?;
        tx.execute("INSERT INTO keys (tag, hash, created_at) VALUES (?1, ?2, ?3)", params![tag, hash_key(&key), now()])?;
        tx.commit()?;
        Ok(key)
    }

    pub fn revoke_key(&self, id: i64) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE keys SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![id, now()],
        )?;
        if n == 0 {
            return Err(ServerError::NotFound(format!("no live key {id}")));
        }
        Ok(())
    }

    pub fn keys(&self, user: Option<&str>) -> Result<Vec<KeyRow>> {
        let mut st = self.conn.prepare(
            "SELECT k.id, u.name, k.tag, k.created_at, k.last_used_at, k.revoked_at
               FROM keys k JOIN tags t ON t.tag = k.tag JOIN users u ON u.id = t.user_id
              WHERE ?1 IS NULL OR u.name = ?1
              ORDER BY u.name, k.tag, k.id",
        )?;
        let rows = st.query_map([user], |r| {
            Ok(KeyRow {
                id: r.get(0)?,
                user: r.get(1)?,
                tag: r.get(2)?,
                created_at: r.get(3)?,
                last_used_at: r.get(4)?,
                revoked_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The principal a bearer key belongs to, if the key is live and its user is not disabled.
    pub fn authenticate(&self, key: &str) -> Result<Principal> {
        let row: Option<(i64, String, String)> = self
            .conn
            .query_row(
                "SELECT k.id, k.tag, u.name
                   FROM keys k JOIN tags t ON t.tag = k.tag JOIN users u ON u.id = t.user_id
                  WHERE k.hash = ?1 AND k.revoked_at IS NULL AND u.disabled_at IS NULL",
                [hash_key(key.trim())],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (key_id, tag, user) = row.ok_or(ServerError::Unauthorized)?;
        // Minute resolution is plenty for "when was this key last seen", and it keeps a
        // chatty client from turning every pull into a write.
        self.conn.execute(
            "UPDATE keys SET last_used_at = ?2 WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at < ?2 - 60)",
            params![key_id, now()],
        )?;
        Ok(Principal { user, tag, key_id })
    }

    pub fn high_water(&self, tag: &str) -> Result<HighWater> {
        let mut hw = HighWater::default();
        let mut st = self.conn.prepare("SELECT kind, value FROM high_water WHERE tag = ?1")?;
        let rows = st.query_map([tag], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (kind, value) = row?;
            match kind.as_str() {
                "outbox" => hw.outbox = value,
                "task" => hw.task = value,
                "note" => hw.note = value,
                "event" => hw.event = value,
                _ => {}
            }
        }
        Ok(hw)
    }
}

// ---------------------------------------------------------------------------
// Rooms
// ---------------------------------------------------------------------------

/// A board name as other people's terminals will show it: no control characters, at most 64
/// characters. Cleaned rather than refused -- a name is a hint, and a client must not fail to
/// join over one.
fn board_label(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

fn parse_room(raw: &str) -> Result<String> {
    protocol::room_name(raw).ok_or_else(|| {
        ServerError::BadRequest(format!(
            "room {raw:?}: up to 64 lowercase letters, digits, '-' or '_', starting with a letter or digit"
        ))
    })
}

impl ServerDb {
    fn room_id(&self, name: &str) -> Result<i64> {
        let name = parse_room(name)?;
        self.conn
            .query_row("SELECT id FROM rooms WHERE name = ?1", [&name], |r| r.get(0))
            .optional()?
            .ok_or_else(|| ServerError::NotFound(format!("no room {name}")))
    }

    /// The room, if the principal is a current member of it. Writing requires having joined, so
    /// membership is what `room list` and the empty-room rule can trust. Reading does not: any
    /// key holder may join any room anyway, and a look before connecting (`room show`) should
    /// not put the looker in the member list.
    fn member_room(&self, name: &str, p: &Principal) -> Result<i64> {
        let id = self.room_id(name)?;
        let member: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM members WHERE room_id = ?1 AND tag = ?2 AND left_at IS NULL)",
            params![id, p.tag],
            |r| r.get(0),
        )?;
        if !member {
            return Err(ServerError::NotFound(format!(
                "not a member of room {name}. Connect a board to it first"
            )));
        }
        Ok(id)
    }

    pub fn room_info(&self, id: i64) -> Result<RoomInfo> {
        let (name, created_at, created_by, board): (String, i64, String, Option<String>) = self.conn.query_row(
            "SELECT name, created_at, created_by, board FROM rooms WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut st = self.conn.prepare(
            "SELECT m.tag, u.name FROM members m JOIN tags t ON t.tag = m.tag JOIN users u ON u.id = t.user_id
              WHERE m.room_id = ?1 AND m.left_at IS NULL ORDER BY m.tag",
        )?;
        let members = st
            .query_map([id], |r| Ok(Member { tag: r.get(0)?, user: r.get(1)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let (changes, head, last_change_at): (i64, i64, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(seq), 0), MAX(received_at) FROM changes WHERE room_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(RoomInfo { name, created_at, created_by, members, changes, head, board, last_change_at })
    }

    pub fn rooms(&self) -> Result<Vec<RoomInfo>> {
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM rooms ORDER BY name")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter().map(|id| self.room_info(id)).collect()
    }

    /// Joins a room, creating it if it does not exist. Any user may create one: a server is a
    /// team. Joining again is harmless.
    pub fn join(&self, name: &str, p: &Principal) -> Result<Joined> {
        self.join_from(name, p, None)
    }

    /// `join`, naming the board the room is created from. Only the join that creates the room
    /// records it: a later member's board name says nothing about where the room came from.
    pub fn join_from(&self, name: &str, p: &Principal, board: Option<&str>) -> Result<Joined> {
        let name = parse_room(name)?;
        let board = board.map(board_label).filter(|b| !b.is_empty());
        let tx = self.conn.unchecked_transaction()?;
        let created = tx.execute(
            "INSERT OR IGNORE INTO rooms (name, created_at, created_by, board) VALUES (?1, ?2, ?3, ?4)",
            params![name, now(), p.user, board],
        )? == 1;
        let id: i64 = tx.query_row("SELECT id FROM rooms WHERE name = ?1", [&name], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO members (room_id, tag, joined_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (room_id, tag) DO UPDATE SET left_at = NULL",
            params![id, p.tag, now()],
        )?;
        tx.commit()?;
        Ok(Joined { room: self.room_info(id)?, created })
    }

    pub fn leave(&self, name: &str, p: &Principal) -> Result<()> {
        let id = self.member_room(name, p)?;
        self.conn.execute(
            "UPDATE members SET left_at = ?3 WHERE room_id = ?1 AND tag = ?2",
            params![id, p.tag, now()],
        )?;
        Ok(())
    }

    /// What deleting a room would remove.
    pub fn room_removal(&self, name: &str) -> Result<Removal> {
        let id = self.room_id(name)?;
        let changes = self.conn.query_row("SELECT COUNT(*) FROM changes WHERE room_id = ?1", [id], |r| r.get(0))?;
        let members = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE room_id = ?1 AND left_at IS NULL",
            [id],
            |r| r.get(0),
        )?;
        Ok(Removal { changes, members })
    }

    pub fn delete_room(&self, name: &str) -> Result<()> {
        let id = self.room_id(name)?;
        let tx = self.conn.unchecked_transaction()?;
        // Explicit rather than trusting the cascade, as core's forget does: a delete that
        // leaves content behind because a pragma did not stick is the failure to avoid.
        tx.execute("DELETE FROM changes WHERE room_id = ?1", [id])?;
        tx.execute("DELETE FROM forgotten WHERE room_id = ?1", [id])?;
        tx.execute("DELETE FROM members WHERE room_id = ?1", [id])?;
        tx.execute("DELETE FROM rooms WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Removes rooms that hold nothing and that nobody is connected to, once they have been
    /// that way for `max_age` seconds. A typo'd `room connect` creates a room; this is what
    /// keeps those from piling up. A room with content is never removed this way, connected
    /// or not -- it may be the only copy of someone's history.
    pub fn remove_empty_rooms(&self, max_age: i64) -> Result<Vec<String>> {
        let cutoff = now() - max_age;
        let names: Vec<String> = {
            let mut st = self.conn.prepare(
                "SELECT r.name FROM rooms r
                  WHERE NOT EXISTS (SELECT 1 FROM changes c WHERE c.room_id = r.id)
                    AND NOT EXISTS (SELECT 1 FROM members m WHERE m.room_id = r.id AND m.left_at IS NULL)
                    AND MAX(r.created_at, COALESCE((SELECT MAX(left_at) FROM members m WHERE m.room_id = r.id), 0)) < ?1
                  ORDER BY r.name",
            )?;
            st.query_map([cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for name in &names {
            self.delete_room(name)?;
        }
        Ok(names)
    }
}

// ---------------------------------------------------------------------------
// The change log
// ---------------------------------------------------------------------------

fn body_hash(body: &str) -> String {
    hash_key(body)
}

impl ServerDb {
    /// Stores a batch, all or nothing.
    ///
    /// What the relay checks without reading values: the change id and the clock carry the
    /// key's own tag, so nobody can write under someone else's name or win a conflict with
    /// someone else's tag. Everything else is stored as sent.
    pub fn push(&self, room: &str, p: &Principal, changes: &[Change]) -> Result<Pushed> {
        if changes.len() > protocol::MAX_BATCH {
            return Err(ServerError::BadRequest(format!(
                "{} changes in one push; the limit is {}",
                changes.len(),
                protocol::MAX_BATCH
            )));
        }
        let room_id = self.member_room(room, p)?;
        for (i, ch) in changes.iter().enumerate() {
            let bad = |why: String| ServerError::BadRequest(format!("change {i} ({}): {why}", ch.cid));
            match ch.cid_parts() {
                Some((tag, _)) if tag == p.tag => {}
                Some((tag, _)) => return Err(bad(format!("change id carries tag {tag}, but this key is {}", p.tag))),
                None => return Err(bad("change id must be <tag>:<positive number>".into())),
            }
            if ch.clock.tag != p.tag {
                return Err(bad(format!("clock carries tag {}, but this key is {}", ch.clock.tag, p.tag)));
            }
            if !ch.entity.valid() {
                return Err(bad(format!("entity must be one of {:?} with a valid origin and a positive id", protocol::KINDS)));
            }
            if ch.subject.as_ref().is_some_and(|s| !s.valid() || s.kind == "event") {
                return Err(bad("subject must be a task or note with a valid origin and a positive id".into()));
            }
        }

        let mut out = Pushed::default();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for ch in changes {
            if self.is_forgotten(room_id, &ch.entity)? || ch.subject.as_ref().map(|s| self.is_forgotten(room_id, s)).transpose()?.unwrap_or(false) {
                out.forgotten += 1;
                continue;
            }
            let body = serde_json::to_string(ch).map_err(|e| ServerError::Other(e.to_string()))?;
            let hash = body_hash(&body);
            let existing: Option<String> = tx
                .query_row("SELECT body_hash FROM changes WHERE cid = ?1", [&ch.cid], |r| r.get(0))
                .optional()?;
            match existing {
                Some(h) if h == hash => {
                    out.duplicates += 1;
                    continue;
                }
                Some(_) => {
                    return Err(ServerError::Conflict(format!(
                        "change id {} was already used for different content. This store looks restored \
                         from a backup or copied, or a second machine sharing one key: it needs a key of its own \
                         (`ai-kanban-relay key add <user>`)",
                        ch.cid
                    )));
                }
                None => {}
            }
            let (s_kind, s_origin, s_id) = match &ch.subject {
                Some(s) => (Some(s.kind.as_str()), Some(s.origin.as_str()), Some(s.id)),
                None => (None, None, None),
            };
            tx.execute(
                "INSERT INTO changes (room_id, cid, author, tag, received_at, entity_kind, entity_origin, entity_id,
                                      subject_kind, subject_origin, subject_id, body, body_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![room_id, ch.cid, p.user, p.tag, now(), ch.entity.kind, ch.entity.origin, ch.entity.id,
                        s_kind, s_origin, s_id, body, hash],
            )?;
            if let Some((_, seq)) = ch.cid_parts() {
                bump(&tx, &p.tag, "outbox", seq)?;
            }
            if ch.entity.origin == p.tag {
                bump(&tx, &p.tag, &ch.entity.kind, ch.entity.id)?;
            }
            out.accepted += 1;
        }
        tx.commit()?;
        out.head = self.head(room_id)?;
        Ok(out)
    }

    fn is_forgotten(&self, room_id: i64, r: &Ref) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM forgotten WHERE room_id = ?1 AND kind = ?2 AND origin = ?3 AND id = ?4)",
            params![room_id, r.kind, r.origin, r.id],
            |row| row.get(0),
        )?)
    }

    pub fn head(&self, room_id: i64) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes WHERE room_id = ?1", [room_id], |r| r.get(0))?)
    }

    /// The head of a room the principal is a member of. What the notification stream starts from.
    pub fn member_head(&self, room: &str, p: &Principal) -> Result<(i64, i64)> {
        let id = self.member_room(room, p)?;
        Ok((id, self.head(id)?))
    }

    /// Open to every key, member or not (see `member_room`). `p` is the authenticated caller.
    pub fn pull(&self, room: &str, _p: &Principal, since: i64, limit: usize) -> Result<Page> {
        let room_id = self.room_id(room)?;
        let limit = limit.clamp(1, protocol::MAX_PAGE);
        let mut st = self.conn.prepare(
            "SELECT seq, author, received_at, body FROM changes WHERE room_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![room_id, since, limit as i64 + 1], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
        })?;
        let mut changes = Vec::new();
        for row in rows {
            let (seq, author, received_at, body) = row?;
            let change: Change = serde_json::from_str(&body).map_err(|e| ServerError::Other(format!("stored change {seq}: {e}")))?;
            changes.push(Pulled { seq, author, received_at, change });
        }
        let more = changes.len() > limit;
        changes.truncate(limit);
        Ok(Page { changes, head: self.head(room_id)?, more })
    }

    /// What forgetting `r` in `room` would remove.
    pub fn forget_removal(&self, room: &str, r: &Ref) -> Result<Removal> {
        let room_id = self.room_id(room)?;
        let changes = self.conn.query_row(
            "SELECT COUNT(*) FROM changes WHERE room_id = ?1 AND
                ((entity_kind = ?2 AND entity_origin = ?3 AND entity_id = ?4) OR
                 (subject_kind = ?2 AND subject_origin = ?3 AND subject_id = ?4))",
            params![room_id, r.kind, r.origin, r.id],
            |row| row.get(0),
        )?;
        Ok(Removal { changes, members: 0 })
    }

    /// Permanently removes a task or note from a room, for every member.
    ///
    /// Deletes every change whose entity or subject it is -- the same set `core::forget`
    /// deletes locally, because the subject was chosen to mirror it. Then it appends a tombstone
    /// carrying no content, signed with the server's tag, so every member applies the forget,
    /// and remembers the subject, so a member who was offline cannot push it back.
    pub fn forget(&self, room: &str, r: &Ref) -> Result<Removal> {
        if r.kind == "event" || !r.valid() {
            return Err(ServerError::BadRequest("only a task or a note can be forgotten".into()));
        }
        let room_id = self.room_id(room)?;
        // Forgetting twice is a no-op, not a second tombstone: nothing is left to remove, and
        // every member already has the first one.
        if self.is_forgotten(room_id, r)? {
            return Ok(Removal::default());
        }
        let removal = self.forget_removal(room, r)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM changes WHERE room_id = ?1 AND
                ((entity_kind = ?2 AND entity_origin = ?3 AND entity_id = ?4) OR
                 (subject_kind = ?2 AND subject_origin = ?3 AND subject_id = ?4))",
            params![room_id, r.kind, r.origin, r.id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO forgotten (room_id, kind, origin, id, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![room_id, r.kind, r.origin, r.id, now()],
        )?;
        let seq = bump_next(&tx, protocol::SERVER_TAG, "outbox")?;
        let tomb = Change {
            cid: format!("{}:{seq}", protocol::SERVER_TAG),
            entity: r.clone(),
            subject: None,
            field: Some("forgotten".into()),
            value: serde_json::Value::Bool(true),
            clock: protocol::Clock {
                wall_ms: now() * 1000,
                counter: 0,
                tag: protocol::SERVER_TAG.into(),
            },
            extra: Default::default(),
        };
        let body = serde_json::to_string(&tomb).map_err(|e| ServerError::Other(e.to_string()))?;
        tx.execute(
            "INSERT INTO changes (room_id, cid, author, tag, received_at, entity_kind, entity_origin, entity_id, body, body_hash)
             VALUES (?1, ?2, 'admin', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![room_id, tomb.cid, protocol::SERVER_TAG, now(), r.kind, r.origin, r.id, body, body_hash(&body)],
        )?;
        tx.commit()?;
        Ok(removal)
    }

    /// A consistent copy of the database, through SQLite so the WAL comes with it (the reason
    /// `core::transfer` exists, applied to the relay).
    pub fn snapshot(&self, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            return Err(ServerError::Other(format!("{} exists", dest.display())));
        }
        self.conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }
}

/// Raises a tag's high-water mark for `kind` to at least `value`.
fn bump(tx: &Connection, tag: &str, kind: &str, value: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO high_water (tag, kind, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (tag, kind) DO UPDATE SET value = MAX(value, excluded.value)",
        params![tag, kind, value],
    )?;
    Ok(())
}

/// The next value of a counter the server owns, for its own change ids.
fn bump_next(tx: &Connection, tag: &str, kind: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO high_water (tag, kind, value) VALUES (?1, ?2, 1)
         ON CONFLICT (tag, kind) DO UPDATE SET value = value + 1",
        params![tag, kind],
    )?;
    Ok(tx.query_row("SELECT value FROM high_water WHERE tag = ?1 AND kind = ?2", params![tag, kind], |r| r.get(0))?)
}
