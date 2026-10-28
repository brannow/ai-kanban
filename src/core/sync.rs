//! The client half of sync, without any network: which board is connected to which room,
//! turning local writes into changes (`flush`), and applying changes from a room (`apply`).
//! `docs/sync.md` is the design; migration 014 holds the state.
//!
//! # How a local write becomes a change
//!
//! Triggers mark every row on a connected board that is inserted or updated as dirty. `flush`
//! reads each dirty row's synced fields, compares them with the last value published to or
//! received from the room (`sync_fields`), and writes a change for every field that differs.
//! So no write path has to remember to record anything -- the failure note #17 describes,
//! where a new field needed five homes and missed them silently.
//!
//! # How a received change lands
//!
//! Written straight into the rows, never through `create_task` or `update_task`: those write
//! their own events (which arrive from the room separately) and their writes would be flushed
//! back out as an echo. A received value is stored in `sync_fields` too, so the next flush
//! sees nothing to publish. Each change applies in its own savepoint, so one this binary
//! cannot store -- a status it does not know yet -- is parked and the rest still apply.

use crate::core::error::{Error, Result};
use crate::core::store::{now, Store};
use crate::sync::protocol::{Change, Clock, Pulled, Ref, SERVER_TAG};
use crate::core::model::Handles;
use rusqlite::types::Value as Sql;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// How a synced column travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enc {
    /// TEXT, or NULL.
    Text,
    /// INTEGER, or NULL.
    Int,
    /// A local task id, sent as the task's identity (`{origin, id}`).
    TaskRef,
    /// A local note id, sent as the note's identity.
    NoteRef,
    /// A local workstream id, sent as its name: workstreams merge by name.
    Workstream,
    /// A note's paths, from `note_paths`, sent as one sorted list.
    Paths,
    /// A task's repos, from `task_repos`, sent as one sorted list of repo keys
    /// (`core::project::repo_key`), plus the ones with no checkout here.
    Repos,
}

/// One synced field: its name on the wire, the column it lives in (empty for `Paths`), and how
/// it is encoded.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub field: &'static str,
    pub column: &'static str,
    pub enc: Enc,
}

const fn spec(field: &'static str, column: &'static str, enc: Enc) -> Spec {
    Spec { field, column, enc }
}

/// Every column of `tasks` is in exactly one of these two lists. `tests/sync_core.rs` reads
/// `PRAGMA table_info` and fails on a column in neither, so a new column cannot silently stay
/// out of sync.
pub const TASK_FIELDS: &[Spec] = &[
    spec("title", "title", Enc::Text),
    spec("body", "body", Enc::Text),
    spec("status", "status", Enc::Text),
    spec("type", "type", Enc::Text),
    spec("origin", "origin", Enc::Text),
    spec("priority", "priority", Enc::Text),
    spec("blocked_by", "blocked_by", Enc::TaskRef),
    spec("workstream", "workstream_id", Enc::Workstream),
    spec("tags", "tags", Enc::Text),
    spec("external_ref", "external_ref", Enc::Text),
    spec("created_at", "created_at", Enc::Int),
    spec("repos", "", Enc::Repos),
];
/// Per machine: the row's local id and board, and bookkeeping each store keeps for itself.
pub const TASK_LOCAL: &[&str] = &["id", "project_id", "updated_at", "version"];

pub const NOTE_FIELDS: &[Spec] = &[
    spec("task", "task_id", Enc::TaskRef),
    spec("title", "title", Enc::Text),
    spec("body", "body", Enc::Text),
    spec("tags", "tags", Enc::Text),
    spec("created_at", "created_at", Enc::Int),
    spec("paths", "", Enc::Paths),
];
pub const NOTE_LOCAL: &[&str] = &["id", "project_id", "updated_at", "version"];

/// Events are insert-only: these travel together as one change whose value is the whole row.
pub const EVENT_FIELDS: &[Spec] = &[
    spec("task", "task_id", Enc::TaskRef),
    spec("note", "note_id", Enc::NoteRef),
    spec("ts", "ts", Enc::Int),
    spec("actor", "actor", Enc::Text),
    spec("kind", "kind", Enc::Text),
    spec("body", "body", Enc::Text),
];
pub const EVENT_LOCAL: &[&str] = &["id", "project_id"];

/// The event kinds a room receives. An allow-list, so a kind added later stays on this
/// machine until someone decides it belongs to the team.
///
/// What it leaves out, and why that is the safe side: `repo_added` carries a local checkout
/// path; `project_created`, `moved` and `moved_out` name local boards ("moved here from board
/// customer-x"); `repo_home`, `repo_renamed`, `repo_removed` and `workstream_closed` describe
/// one machine's setup; `workstream_created` is written by each machine when a workstream
/// first appears there; `forgotten` and `left_room` are written locally when a tombstone or a
/// departure arrives; the housekeeping kinds say nothing about the work.
pub const PUBLISHED_KINDS: &[&str] = &["created", "updated", "note_added", "note_updated", "log"];

/// Whether events of `kind` are shared with a room. `status:<s>` is a task changing status.
pub fn published_kind(kind: &str) -> bool {
    PUBLISHED_KINDS.contains(&kind) || kind.starts_with("status:")
}

/// Marks an event as published to a room, in `sync_fields`.
const ROW: &str = "_row";
/// The field a task carries when it leaves a room.
const LEFT_ROOM: &str = "left_room";
/// The field of the relay's forget tombstone.
const FORGOTTEN: &str = "forgotten";

fn specs(kind: &str) -> &'static [Spec] {
    match kind {
        "task" => TASK_FIELDS,
        "note" => NOTE_FIELDS,
        _ => EVENT_FIELDS,
    }
}

fn table(kind: &str) -> &'static str {
    match kind {
        "task" => "tasks",
        "note" => "notes",
        _ => "events",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What one `apply` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub applied: usize,
    /// Older than what is already here, or this store's own change coming back.
    pub ignored: usize,
    /// Could not be stored by this binary; kept for re-applying after an upgrade.
    pub parked: usize,
}

/// Whether this run may use the network. `AI_KANBAN_SYNC=off` keeps one Claude run (or shell)
/// local-only without changing anything stored. Defined here, once, because the board header
/// says so too; `remote::config::sync_enabled` is this.
pub fn sync_enabled() -> bool {
    !std::env::var("AI_KANBAN_SYNC").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

/// A connected board's sync state, for `room status` and the board header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncStatus {
    pub room: String,
    pub cursor: i64,
    /// Changes waiting to be pushed: per field, so one new task is a dozen of them.
    pub unsent: i64,
    /// What those changes are about, for people: (tasks, notes, board-level events).
    pub unsent_items: (i64, i64, i64),
    pub parked: i64,
    pub last_ok_at: Option<i64>,
    /// Why the last attempt failed, until one succeeds.
    pub last_error: Option<String>,
    /// Why sync is stopped for this whole store, if it is.
    pub paused: Option<String>,
    /// (tag, user) of everyone in the room, as last reported.
    pub members: Vec<(String, String)>,
    /// This run has sync off (`AI_KANBAN_SYNC`): nothing reaches the relay until a run allows it.
    pub off: bool,
    /// This store's own tag, so a reader can tell "you" from the team.
    pub me: Option<String>,
    /// Who has each task in `doing`: (local task id, origin tag), one entry per claimer.
    pub claims: Vec<(i64, String)>,
}

impl SyncStatus {
    /// What to call whoever publishes as `tag`: "you" for this store, the member's name when the
    /// relay has told us, the bare tag otherwise.
    pub fn name_of(&self, tag: &str) -> String {
        if self.me.as_deref() == Some(tag) {
            return "you".into();
        }
        self.members.iter().find(|(t, _)| t == tag).map(|(_, u)| u.clone()).unwrap_or_else(|| tag.to_string())
    }

    /// Who has a task in `doing`, by name: "volker", or "volker, you" when two are on it.
    pub fn claimed_by(&self, task_id: i64) -> Option<String> {
        let names: Vec<String> = self.claims.iter().filter(|(id, _)| *id == task_id).map(|(_, tag)| self.name_of(tag)).collect();
        (!names.is_empty()).then(|| names.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Identity, clock, connection
// ---------------------------------------------------------------------------

impl Store {
    /// The origin tag this store publishes under, once a key has been set up.
    pub fn origin_tag(&self) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT tag FROM sync_node WHERE id = 1", [], |r| r.get(0))?)
    }

    /// Sets the origin tag. Refused while a board is connected: changes already published
    /// carry the old tag, and rows this store created keep it (`sync_ids`), so changing it
    /// mid-room would mix two identities for one machine.
    pub fn set_origin_tag(&self, tag: &str) -> Result<()> {
        if !crate::sync::protocol::valid_tag(tag) || tag == SERVER_TAG {
            return Err(Error::InvalidValue { field: "tag", value: tag.into(), valid: "a tag issued with your key, like b1".into() });
        }
        let connected: i64 = self.conn.query_row("SELECT COUNT(*) FROM sync_rooms", [], |r| r.get(0))?;
        if connected > 0 && self.origin_tag()?.as_deref() != Some(tag) {
            return Err(Error::Other("disconnect every board before changing this store's origin tag".into()));
        }
        self.conn.execute("UPDATE sync_node SET tag = ?1 WHERE id = 1", [tag])?;
        self.conn.execute("INSERT OR IGNORE INTO sync_own_tags (tag) VALUES (?1)", [tag])?;
        Ok(())
    }

    /// Whether `tag` is one this store has published under, now or before a restore.
    pub fn is_own_tag(&self, tag: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sync_own_tags WHERE tag = ?1)", [tag], |r| r.get(0))?)
    }

    /// The next clock value: never behind the wall clock, never behind anything seen before.
    fn tick(&self, tag: &str) -> Result<Clock> {
        let (wall, counter): (i64, i64) =
            self.conn.query_row("SELECT wall_ms, counter FROM sync_node WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let now = now_ms();
        let (wall, counter) = if now > wall { (now, 0) } else { (wall, counter + 1) };
        self.conn.execute("UPDATE sync_node SET wall_ms = ?1, counter = ?2 WHERE id = 1", params![wall, counter])?;
        Ok(Clock { wall_ms: wall, counter: counter as u32, tag: tag.into() })
    }

    /// Moves the clock past a received one. Without this, an edit made here right after
    /// receiving a teammate's could lose to it when their machine's clock runs ahead.
    fn observe(&self, c: &Clock) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_node SET
                counter = CASE WHEN ?1 > wall_ms THEN ?2 WHEN ?1 = wall_ms THEN MAX(counter, ?2) ELSE counter END,
                wall_ms = MAX(wall_ms, ?1)
              WHERE id = 1",
            params![c.wall_ms, c.counter],
        )?;
        Ok(())
    }

    pub fn room_of(&self, project_id: i64) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT room FROM sync_rooms WHERE project_id = ?1", [project_id], |r| r.get(0)).optional()?)
    }

    pub fn board_of_room(&self, room: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT project_id FROM sync_rooms WHERE room = ?1", [room], |r| r.get(0)).optional()?)
    }

    /// Connects a board to a room: from here on everything on it is published, starting with
    /// everything already there. One room per board and one board per room.
    pub fn connect_board(&self, project_id: i64, room: &str) -> Result<()> {
        let room = crate::sync::protocol::room_name(room)
            .ok_or_else(|| Error::InvalidValue { field: "room", value: room.into(), valid: "lowercase letters, digits, '-' and '_'".into() })?;
        self.project(project_id)?;
        if self.origin_tag()?.is_none() {
            return Err(Error::Other("no origin tag yet: set up the remote first".into()));
        }
        if let Some(r) = self.room_of(project_id)? {
            return Err(Error::Other(format!("this board is already connected to room {r}")));
        }
        if let Some(other) = self.board_of_room(&room)? {
            let name = self.project(other)?.name;
            return Err(Error::Other(format!("room {room} is already connected to board {name} on this machine")));
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO sync_rooms (project_id, room, connected_at) VALUES (?1, ?2, ?3)", params![project_id, room, now()])?;
        // The whole board is the first publish: the same path as any later edit.
        for (kind, t) in [("task", "tasks"), ("note", "notes"), ("event", "events")] {
            tx.execute(
                &format!("INSERT OR IGNORE INTO sync_dirty (kind, local_id) SELECT '{kind}', id FROM {t} WHERE project_id = ?1"),
                [project_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Uncouples a board from its room. Unsent changes are dropped -- the CLI asks to sync them
    /// first. With `wipe_foreign`, rows other members created are removed from this machine;
    /// the room still holds them. Rows that stay keep their identity, so connecting again
    /// matches them rather than duplicating them.
    pub fn disconnect_board(&self, project_id: i64, wipe_foreign: bool) -> Result<()> {
        if self.room_of(project_id)?.is_none() {
            return Err(Error::Other("this board is not connected to a room".into()));
        }
        let room = self.room_of(project_id)?.unwrap_or_default();
        let tx = self.conn.unchecked_transaction()?;
        // A dropped change was recorded as published. Unrecord it, as long as nothing newer has
        // replaced it, or reconnecting would find "nothing changed" and the edit would never
        // reach the room.
        for (_, c) in self.outbox(project_id, usize::MAX)? {
            let field = c.field.as_deref().unwrap_or(ROW);
            tx.execute(
                "DELETE FROM sync_fields WHERE room = ?1 AND kind = ?2 AND origin = ?3 AND origin_id = ?4 AND field = ?5
                    AND wall_ms = ?6 AND counter = ?7 AND tag = ?8",
                params![room, c.entity.kind, c.entity.origin, c.entity.id, field, c.clock.wall_ms, c.clock.counter, c.clock.tag],
            )?;
        }
        tx.execute("DELETE FROM sync_rooms WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM sync_room_status WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM sync_outbox WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM sync_parked WHERE project_id = ?1", [project_id])?;
        tx.commit()?;
        // Only tasks other members created, with their history and the notes others attached
        // to them. Teammates' findings on this store's own tasks stay: they are the reason own
        // tasks sync both ways. "Other members" means any tag this store never published under,
        // so work from before a restore (and a new tag) is still its own.
        if wipe_foreign {
            let foreign_tasks: Vec<i64> = {
                let mut st = self.conn.prepare(
                    "SELECT t.id FROM tasks t JOIN sync_ids s ON s.kind = 'task' AND s.local_id = t.id
                      WHERE t.project_id = ?1 AND s.origin NOT IN (SELECT tag FROM sync_own_tags) ORDER BY t.id",
                )?;
                let ids = st.query_map([project_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
                ids
            };
            for id in foreign_tasks {
                let their_notes: Vec<i64> = {
                    let mut st = self.conn.prepare(
                        "SELECT n.id FROM notes n JOIN sync_ids s ON s.kind = 'note' AND s.local_id = n.id
                          WHERE n.task_id = ?1 AND s.origin NOT IN (SELECT tag FROM sync_own_tags)",
                    )?;
                    let ids = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
                    ids
                };
                for n in their_notes {
                    self.forget_note_unchecked(project_id, n, false)?;
                }
                self.forget_task_unchecked(project_id, id, false)?;
            }
        }
        Ok(())
    }

    /// Tolerant like every other read the hook can reach: `None` on a store older than the
    /// tables it reads.
    pub fn sync_status(&self, project_id: i64) -> Result<Option<SyncStatus>> {
        let row = self.conn.query_row(
            "SELECT r.room, r.cursor, s.last_ok_at, s.last_error, COALESCE(s.members, '[]')
               FROM sync_rooms r LEFT JOIN sync_room_status s ON s.project_id = r.project_id
              WHERE r.project_id = ?1",
            [project_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, String>(4)?)),
        );
        let (room, cursor, last_ok_at, last_error, members) = match row.optional() {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(e) if crate::core::workstream::is_missing_schema(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let unsent = self.conn.query_row("SELECT COUNT(*) FROM sync_outbox WHERE project_id = ?1", [project_id], |r| r.get(0))?;
        // By subject: a task's fields and its history count as that task, a note's as that note.
        let unsent_items = self.conn.query_row(
            "SELECT COUNT(DISTINCT CASE WHEN json_extract(body, '$.subject.kind') = 'task'
                                        THEN json_extract(body, '$.subject.origin') || ':' || json_extract(body, '$.subject.id') END),
                    COUNT(DISTINCT CASE WHEN json_extract(body, '$.subject.kind') = 'note'
                                        THEN json_extract(body, '$.subject.origin') || ':' || json_extract(body, '$.subject.id') END),
                    COALESCE(SUM(json_extract(body, '$.subject') IS NULL), 0)
               FROM sync_outbox WHERE project_id = ?1",
            [project_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let parked = self.conn.query_row("SELECT COUNT(*) FROM sync_parked WHERE project_id = ?1", [project_id], |r| r.get(0))?;
        let members: Vec<Value> = serde_json::from_str(&members).unwrap_or_default();
        let members = members.iter().filter_map(|m| Some((m.get("tag")?.as_str()?.to_string(), m.get("user")?.as_str()?.to_string()))).collect();
        let claims = self.claims(project_id)?.into_iter().map(|(id, c)| (id, c.tag)).collect();
        Ok(Some(SyncStatus {
            room, cursor, unsent, unsent_items, parked, last_ok_at, last_error, paused: self.paused()?, members,
            off: !sync_enabled(), me: self.origin_tag()?, claims,
        }))
    }

    /// Why sync is stopped for this store, if it is.
    pub fn paused(&self) -> Result<Option<String>> {
        match self.conn.query_row("SELECT reason FROM sync_pause WHERE id = 1", [], |r| r.get(0)).optional() {
            Ok(p) => Ok(p),
            Err(e) if crate::core::workstream::is_missing_schema(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_paused(&self, reason: Option<&str>) -> Result<()> {
        match reason {
            Some(r) => self.conn.execute("INSERT INTO sync_pause (id, reason) VALUES (1, ?1) ON CONFLICT (id) DO UPDATE SET reason = excluded.reason", [r])?,
            None => self.conn.execute("DELETE FROM sync_pause", [])?,
        };
        Ok(())
    }

    /// Records a successful round with the relay, and who is in the room.
    pub fn sync_succeeded(&self, project_id: i64, members: &[(String, String)]) -> Result<()> {
        let members: Vec<Value> = members.iter().map(|(t, u)| json!({ "tag": t, "user": u })).collect();
        self.conn.execute(
            "INSERT INTO sync_room_status (project_id, last_ok_at, last_error, members) VALUES (?1, ?2, NULL, ?3)
             ON CONFLICT (project_id) DO UPDATE SET last_ok_at = excluded.last_ok_at, last_error = NULL, members = excluded.members",
            params![project_id, now(), Value::Array(members).to_string()],
        )?;
        Ok(())
    }

    pub fn sync_failed(&self, project_id: i64, why: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_room_status (project_id, last_error) VALUES (?1, ?2)
             ON CONFLICT (project_id) DO UPDATE SET last_error = excluded.last_error",
            params![project_id, why],
        )?;
        Ok(())
    }

    /// The counters this store would hand out next, in the shape the relay reports its
    /// high-water marks in. A relay that has seen higher ones from this tag means the store was
    /// restored from a backup or copied: its next ids would collide with ones the room holds.
    pub fn local_high_water(&self) -> Result<crate::sync::protocol::HighWater> {
        let seq = |name: &str| -> Result<i64> {
            Ok(self.conn.query_row("SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = ?1), 0)", [name], |r| r.get(0))?)
        };
        let top = |table: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                &format!("SELECT MAX(COALESCE((SELECT MAX(id) FROM {table}), 0), COALESCE((SELECT value FROM id_floor WHERE tbl = '{table}'), 0))"),
                [], |r| r.get(0),
            )?)
        };
        Ok(crate::sync::protocol::HighWater { outbox: seq("sync_outbox")?, task: top("tasks")?, note: top("notes")?, event: seq("events")? })
    }

    pub fn connected_boards(&self) -> Result<Vec<(i64, String)>> {
        let mut st = match self.conn.prepare("SELECT project_id, room FROM sync_rooms ORDER BY project_id") {
            Ok(st) => st,
            Err(e) if crate::core::workstream::is_missing_schema(&e) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The identity a local row has in every room, if it has one yet.
    pub fn identity(&self, kind: &str, local_id: i64) -> Result<Option<Ref>> {
        Ok(self.conn
            .query_row("SELECT origin, origin_id FROM sync_ids WHERE kind = ?1 AND local_id = ?2", params![kind, local_id], |r| {
                Ok(Ref { kind: kind.into(), origin: r.get(0)?, id: r.get(1)? })
            })
            .optional()?)
    }

    /// The local row an identity maps to on this machine.
    pub fn local_id(&self, r: &Ref) -> Result<Option<i64>> {
        Ok(self.conn
            .query_row("SELECT local_id FROM sync_ids WHERE kind = ?1 AND origin = ?2 AND origin_id = ?3", params![r.kind, r.origin, r.id], |row| row.get(0))
            .optional()?)
    }

    /// Changes waiting for `project_id`'s room, oldest first.
    pub fn outbox(&self, project_id: i64, limit: usize) -> Result<Vec<(i64, Change)>> {
        let mut st = self.conn.prepare("SELECT seq, body FROM sync_outbox WHERE project_id = ?1 ORDER BY seq LIMIT ?2")?;
        let rows = st.query_map(params![project_id, limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, body) = row?;
            out.push((seq, serde_json::from_str(&body).map_err(|e| Error::Other(format!("outbox {seq}: {e}")))?));
        }
        Ok(out)
    }

    /// Drops changes the relay has accepted, up to and including `seq`.
    pub fn outbox_sent(&self, project_id: i64, seq: i64) -> Result<()> {
        self.conn.execute("DELETE FROM sync_outbox WHERE project_id = ?1 AND seq <= ?2", params![project_id, seq])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reading and writing synced fields
// ---------------------------------------------------------------------------

impl Store {
    fn ref_value(&self, kind: &str, local: Option<i64>) -> Result<Value> {
        let Some(id) = local else { return Ok(Value::Null) };
        Ok(match self.identity(kind, id)? {
            Some(r) => json!({ "origin": r.origin, "id": r.id }),
            None => Value::Null,
        })
    }

    /// A row's synced fields, encoded for the wire, or `None` if the row is gone.
    fn read_fields(&self, kind: &str, local_id: i64) -> Result<Option<(i64, BTreeMap<&'static str, Value>)>> {
        let specs = specs(kind);
        let cols: Vec<&str> = specs.iter().filter(|s| !s.column.is_empty()).map(|s| s.column).collect();
        debug_assert!(specs.iter().all(|s| s.column.is_empty() == matches!(s.enc, Enc::Paths | Enc::Repos)));
        let sql = format!("SELECT project_id, {} FROM {} WHERE id = ?1", cols.join(", "), table(kind));
        let row: Option<(i64, Vec<Sql>)> = self.conn
            .query_row(&sql, [local_id], |r| {
                let mut v = Vec::with_capacity(cols.len());
                for i in 0..cols.len() {
                    v.push(r.get::<_, Sql>(i + 1)?);
                }
                Ok((r.get(0)?, v))
            })
            .optional()?;
        let Some((project_id, raw)) = row else { return Ok(None) };

        let mut out = BTreeMap::new();
        let mut raw = raw.into_iter();
        for s in specs {
            let value = match s.enc {
                Enc::Paths => {
                    let mut st = self.conn.prepare("SELECT path FROM note_paths WHERE note_id = ?1 ORDER BY path")?;
                    let paths = st.query_map([local_id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                    json!(paths)
                }
                Enc::Repos => json!(self.task_repo_keys(local_id)?),
                enc => {
                    let v = raw.next().unwrap_or(Sql::Null);
                    match (enc, v) {
                        (_, Sql::Null) => Value::Null,
                        (Enc::Text, Sql::Text(t)) => Value::String(t),
                        (Enc::Text, Sql::Integer(i)) => Value::String(i.to_string()),
                        (Enc::Int, Sql::Integer(i)) => json!(i),
                        (Enc::TaskRef, Sql::Integer(i)) => self.ref_value("task", Some(i))?,
                        (Enc::NoteRef, Sql::Integer(i)) => self.ref_value("note", Some(i))?,
                        (Enc::Workstream, Sql::Integer(i)) => self.conn
                            .query_row("SELECT name FROM workstreams WHERE id = ?1", [i], |r| r.get::<_, String>(0))
                            .optional()?
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                        (_, other) => return Err(Error::Other(format!("{kind}.{}: unexpected {other:?}", s.field))),
                    }
                }
            };
            out.insert(s.field, value);
        }
        Ok(Some((project_id, out)))
    }

    /// The local row for an identity, creating a minimal one if it has not arrived yet.
    ///
    /// References can arrive before what they point at -- an event before its task, a
    /// `blocked_by` before the blocker. The minimal row gets every field recorded at clock zero,
    /// so the first real value for each field wins and a flush in between publishes nothing.
    fn local_or_placeholder(&self, room: &str, project_id: i64, r: &Ref) -> Result<i64> {
        if let Some(id) = self.local_id(r)? {
            return Ok(id);
        }
        let id = match r.kind.as_str() {
            "task" => {
                self.conn.execute(
                    "INSERT INTO tasks (project_id, title, created_at, updated_at) VALUES (?1, '', ?2, ?2)",
                    params![project_id, now()],
                )?;
                self.fresh_id("tasks", self.conn.last_insert_rowid())?
            }
            "note" => {
                self.conn.execute(
                    "INSERT INTO notes (project_id, title, created_at, updated_at) VALUES (?1, '', ?2, ?2)",
                    params![project_id, now()],
                )?;
                self.fresh_id("notes", self.conn.last_insert_rowid())?
            }
            other => return Err(Error::Other(format!("cannot create a placeholder {other}"))),
        };
        self.conn.execute(
            "INSERT INTO sync_ids (kind, local_id, origin, origin_id) VALUES (?1, ?2, ?3, ?4)",
            params![r.kind, id, r.origin, r.id],
        )?;
        let (_, fields) = self.read_fields(&r.kind, id)?.ok_or_else(|| Error::Other("placeholder vanished".into()))?;
        let zero = Clock { wall_ms: 0, counter: 0, tag: String::new() };
        for (field, value) in fields {
            self.store_field(room, r, field, &value, &zero)?;
        }
        Ok(id)
    }

    fn decode_ref(&self, room: &str, project_id: i64, kind: &str, v: &Value) -> Result<Option<i64>> {
        if v.is_null() {
            return Ok(None);
        }
        let origin = v.get("origin").and_then(Value::as_str);
        let id = v.get("id").and_then(Value::as_i64);
        match (origin, id) {
            (Some(o), Some(i)) if crate::sync::protocol::valid_tag(o) && i > 0 => {
                let r = Ref { kind: kind.into(), origin: o.into(), id: i };
                if self.is_forgotten(room, &r)? {
                    return Ok(None);
                }
                Ok(Some(self.local_or_placeholder(room, project_id, &r)?))
            }
            _ => Err(Error::Other(format!("not a {kind} reference: {v}"))),
        }
    }

    /// Writes one received field into its row.
    fn write_field(&self, room: &str, project_id: i64, kind: &str, local_id: i64, s: &Spec, v: &Value) -> Result<()> {
        let t = table(kind);
        let bad = || Error::Other(format!("{kind}.{}: unexpected value {v}", s.field));
        let sql_value: Sql = match s.enc {
            Enc::Repos => {
                let keys: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| bad())?;
                self.set_task_repo_keys(project_id, local_id, &keys)?;
                return Ok(());
            }
            Enc::Paths => {
                let paths: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| bad())?;
                self.conn.execute("DELETE FROM note_paths WHERE note_id = ?1", [local_id])?;
                for p in paths {
                    self.conn.execute("INSERT OR IGNORE INTO note_paths (note_id, path) VALUES (?1, ?2)", params![local_id, p])?;
                }
                return Ok(());
            }
            Enc::Text => match v {
                Value::Null => Sql::Null,
                Value::String(x) => Sql::Text(x.clone()),
                _ => return Err(bad()),
            },
            Enc::Int => match v {
                Value::Null => Sql::Null,
                _ => Sql::Integer(v.as_i64().ok_or_else(bad)?),
            },
            Enc::TaskRef => self.decode_ref(room, project_id, "task", v)?.map(Sql::Integer).unwrap_or(Sql::Null),
            Enc::NoteRef => self.decode_ref(room, project_id, "note", v)?.map(Sql::Integer).unwrap_or(Sql::Null),
            // Merged by name: a workstream two members created offline is one workstream.
            Enc::Workstream => match v {
                Value::Null => Sql::Null,
                Value::String(name) => Sql::Integer(self.ensure_workstream(project_id, name)?.id),
                _ => return Err(bad()),
            },
        };
        self.conn.execute(&format!("UPDATE {t} SET {} = ?1 WHERE id = ?2", s.column), params![sql_value, local_id])?;
        Ok(())
    }

    fn stored_field(&self, room: &str, r: &Ref, field: &str) -> Result<Option<(String, Clock)>> {
        Ok(self.conn
            .query_row(
                "SELECT value, wall_ms, counter, tag FROM sync_fields
                  WHERE room = ?1 AND kind = ?2 AND origin = ?3 AND origin_id = ?4 AND field = ?5",
                params![room, r.kind, r.origin, r.id, field],
                |row| Ok((row.get(0)?, Clock { wall_ms: row.get(1)?, counter: row.get::<_, i64>(2)? as u32, tag: row.get(3)? })),
            )
            .optional()?)
    }

    fn store_field(&self, room: &str, r: &Ref, field: &str, value: &Value, c: &Clock) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_fields (room, kind, origin, origin_id, field, value, wall_ms, counter, tag)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (room, kind, origin, origin_id, field) DO UPDATE SET
                value = excluded.value, wall_ms = excluded.wall_ms, counter = excluded.counter, tag = excluded.tag",
            params![room, r.kind, r.origin, r.id, field, value.to_string(), c.wall_ms, c.counter, c.tag],
        )?;
        Ok(())
    }

    fn is_forgotten(&self, room: &str, r: &Ref) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_forgotten WHERE room = ?1 AND kind = ?2 AND origin = ?3 AND origin_id = ?4)",
            params![room, r.kind, r.origin, r.id],
            |row| row.get(0),
        )?)
    }
}

// ---------------------------------------------------------------------------
// Flush: local writes -> outbox
// ---------------------------------------------------------------------------

impl Store {
    /// Turns everything written on connected boards since the last flush into changes in the
    /// outbox, and returns how many. Safe to call from any process at any time.
    pub fn flush(&self) -> Result<usize> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let n = self.flush_inner()?;
        tx.commit()?;
        Ok(n)
    }

    fn flush_inner(&self) -> Result<usize> {
        let dirty: Vec<(String, i64, Option<i64>, Option<i64>)> = {
            // Tasks, then notes, then events: a receiver then usually meets a thing before the
            // things that point at it. Not required -- placeholders cover the other order.
            // `wall_ms` is when the row was last edited (017); rows marked by `connect` have none.
            let mut st = self.conn.prepare(
                "SELECT d.kind, d.local_id, d.left_from, e.wall_ms FROM sync_dirty d
                   LEFT JOIN sync_edited e ON e.kind = d.kind AND e.local_id = d.local_id
                  ORDER BY CASE d.kind WHEN 'task' THEN 0 WHEN 'note' THEN 1 ELSE 2 END, d.local_id",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        self.conn.execute_batch("DELETE FROM sync_dirty; DELETE FROM sync_edited;")?;
        let Some(tag) = self.origin_tag()? else { return Ok(0) };

        // Identities first, so a reference between two rows published in the same flush
        // resolves no matter which comes first.
        for (kind, id, _, _) in &dirty {
            let on_connected: bool = self.conn.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {} x JOIN sync_rooms r ON r.project_id = x.project_id WHERE x.id = ?1)", table(kind)),
                [id],
                |r| r.get(0),
            )?;
            if on_connected {
                self.conn.execute(
                    "INSERT OR IGNORE INTO sync_ids (kind, local_id, origin, origin_id) VALUES (?1, ?2, ?3, ?2)",
                    params![kind, id, tag],
                )?;
            }
        }

        let mut written = 0;
        for (kind, id, left_from, edited) in dirty {
            if let Some(from) = left_from {
                if let (Some(room), Some(me)) = (self.room_of(from)?, self.identity("task", id)?) {
                    let subject = Some(me.clone());
                    written += self.emit(&tag, edited, from, &room, &me, subject, Some(LEFT_ROOM), json!(true))?;
                }
            }
            let Some((project_id, fields)) = self.read_fields(&kind, id)? else { continue };
            let Some(room) = self.room_of(project_id)? else { continue };
            let Some(me) = self.identity(&kind, id)? else { continue };
            if self.is_forgotten(&room, &me)? {
                continue;
            }
            if kind == "event" {
                let ev_kind = fields.get("kind").and_then(Value::as_str).unwrap_or("");
                if !published_kind(ev_kind) || self.stored_field(&room, &me, ROW)?.is_some() {
                    continue;
                }
                let subject = self.event_subject(&fields);
                let row: serde_json::Map<String, Value> = fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
                written += self.emit(&tag, edited, project_id, &room, &me, subject, None, Value::Object(row))?;
                continue;
            }
            for (field, value) in fields {
                let same = self.stored_field(&room, &me, field)?.is_some_and(|(v, _)| v == value.to_string());
                if !same {
                    written += self.emit(&tag, edited, project_id, &room, &me, Some(me.clone()), Some(field), value)?;
                }
            }
        }
        Ok(written)
    }

    /// An event's subject, as `core::forget` defines it: the note if it is about one (note
    /// history survives forgetting the task), else its task, else none.
    fn event_subject(&self, fields: &BTreeMap<&'static str, Value>) -> Option<Ref> {
        let as_ref = |kind: &str, v: Option<&Value>| -> Option<Ref> {
            let v = v?;
            Some(Ref { kind: kind.into(), origin: v.get("origin")?.as_str()?.into(), id: v.get("id")?.as_i64()? })
        };
        as_ref("note", fields.get("note")).or_else(|| as_ref("task", fields.get("task")))
    }

    /// Queues one change, stamped with when the row was edited, so the newest edit wins a
    /// conflict rather than whichever side syncs last (#1069). `tick` only for rows with no edit
    /// recorded: ones `connect` publishes for the first time.
    fn emit(&self, tag: &str, edited: Option<i64>, project_id: i64, room: &str, entity: &Ref, subject: Option<Ref>, field: Option<&str>, value: Value) -> Result<usize> {
        let clock = match edited {
            Some(wall_ms) => Clock { wall_ms, counter: 0, tag: tag.into() },
            None => self.tick(tag)?,
        };
        self.conn.execute("INSERT INTO sync_outbox (project_id, room, body) VALUES (?1, ?2, '')", params![project_id, room])?;
        let seq = self.conn.last_insert_rowid();
        let stored = match field {
            Some(f) => f,
            None => ROW,
        };
        self.store_field(room, entity, stored, &value, &clock)?;
        let change = Change {
            cid: format!("{tag}:{seq}"),
            entity: entity.clone(),
            subject,
            field: field.map(String::from),
            value,
            clock,
            extra: Default::default(),
        };
        let body = serde_json::to_string(&change).map_err(|e| Error::Other(e.to_string()))?;
        self.conn.execute("UPDATE sync_outbox SET body = ?1 WHERE seq = ?2", params![body, seq])?;
        Ok(1)
    }
}

// ---------------------------------------------------------------------------
// Apply: room -> local rows
// ---------------------------------------------------------------------------

enum Outcome {
    Applied,
    Ignored,
}

impl Store {
    /// Applies changes pulled from `project_id`'s room and moves its cursor past them.
    ///
    /// Pending local writes are flushed first, in the same transaction, so nothing received
    /// can overwrite a local edit that has not been published yet.
    pub fn apply(&self, project_id: i64, pulled: &[Pulled]) -> Result<Applied> {
        let room = self.room_of(project_id)?.ok_or_else(|| Error::Other("this board is not connected to a room".into()))?;
        let tag = self.origin_tag()?.unwrap_or_default();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        self.flush_inner()?;
        let mut out = Applied::default();
        let mut cursor = None;
        for p in pulled {
            cursor = Some(cursor.map_or(p.seq, |c: i64| c.max(p.seq)));
            self.apply_one(&tag, &room, project_id, p, &mut out)?;
        }
        if let Some(c) = cursor {
            self.conn.execute("UPDATE sync_rooms SET cursor = MAX(cursor, ?2) WHERE project_id = ?1", params![project_id, c])?;
        }
        // What applying wrote is already in `sync_fields`; flushing it would find no difference.
        // Clearing here just saves the next flush the reads.
        self.conn.execute_batch("DELETE FROM sync_dirty; DELETE FROM sync_edited;")?;
        tx.commit()?;
        Ok(out)
    }

    /// Tries the parked changes again, after an upgrade taught this binary what they need.
    pub fn reapply_parked(&self, project_id: i64) -> Result<Applied> {
        let parked: Vec<(i64, String)> = {
            let mut st = self.conn.prepare("SELECT id, body FROM sync_parked WHERE project_id = ?1 ORDER BY id")?;
            st.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let mut pulled = Vec::new();
        for (id, body) in parked {
            self.conn.execute("DELETE FROM sync_parked WHERE id = ?1", [id])?;
            pulled.push(serde_json::from_str::<Pulled>(&body).map_err(|e| Error::Other(e.to_string()))?);
        }
        // Not through `apply`, whose cursor update would be harmless but whose name would lie.
        let room = self.room_of(project_id)?.ok_or_else(|| Error::Other("this board is not connected to a room".into()))?;
        let tag = self.origin_tag()?.unwrap_or_default();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        self.flush_inner()?;
        let mut out = Applied::default();
        for p in &pulled {
            self.apply_one(&tag, &room, project_id, p, &mut out)?;
        }
        self.conn.execute_batch("DELETE FROM sync_dirty; DELETE FROM sync_edited;")?;
        tx.commit()?;
        Ok(out)
    }

    fn apply_one(&self, tag: &str, room: &str, project_id: i64, p: &Pulled, out: &mut Applied) -> Result<()> {
        let c = &p.change;
        // This store's own change coming back: it is already here.
        if c.cid_parts().is_some_and(|(t, _)| t == tag) {
            out.ignored += 1;
            return Ok(());
        }
        self.observe(&c.clock)?;
        self.conn.execute_batch("SAVEPOINT sync_apply")?;
        match self.apply_change(room, project_id, p) {
            Ok(Outcome::Applied) => {
                self.conn.execute_batch("RELEASE sync_apply")?;
                out.applied += 1;
            }
            Ok(Outcome::Ignored) => {
                self.conn.execute_batch("RELEASE sync_apply")?;
                out.ignored += 1;
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK TO sync_apply; RELEASE sync_apply")?;
                let body = serde_json::to_string(p).map_err(|e| Error::Other(e.to_string()))?;
                self.conn.execute(
                    "INSERT INTO sync_parked (project_id, body, reason, at) VALUES (?1, ?2, ?3, ?4)",
                    params![project_id, body, e.to_string(), now()],
                )?;
                out.parked += 1;
            }
        }
        Ok(())
    }

    fn apply_change(&self, room: &str, project_id: i64, p: &Pulled) -> Result<Outcome> {
        let c = &p.change;
        let e = &c.entity;
        if self.is_forgotten(room, e)? || c.subject.as_ref().map(|s| self.is_forgotten(room, s)).transpose()?.unwrap_or(false) {
            return Ok(Outcome::Ignored);
        }

        if c.field.as_deref() == Some(FORGOTTEN) && c.clock.tag == SERVER_TAG {
            return self.apply_forget(room, project_id, e);
        }

        if e.kind == "event" {
            if c.field.is_some() {
                return Err(Error::Other("an event change carries a field".into()));
            }
            if self.local_id(e)?.is_some() {
                return Ok(Outcome::Ignored);
            }
            let row = |k: &str| c.value.get(k).cloned().unwrap_or(Value::Null);
            if self.points_elsewhere(project_id, Enc::TaskRef, &row("task"))?
                || self.points_elsewhere(project_id, Enc::NoteRef, &row("note"))?
            {
                return Ok(Outcome::Ignored);
            }
            return self.insert_event(room, project_id, e, &c.value, &c.clock);
        }

        let field = c.field.as_deref().ok_or_else(|| Error::Other(format!("a {} change without a field", e.kind)))?;
        if e.kind == "task" && field == LEFT_ROOM {
            return self.apply_left(project_id, e, &p.author);
        }
        let s = specs(&e.kind)
            .iter()
            .find(|s| s.field == field)
            .ok_or_else(|| Error::Other(format!("unknown field {}.{field}", e.kind)))?;
        if let Some((_, stored)) = self.stored_field(room, e, field)? {
            if stored >= c.clock {
                return Ok(Outcome::Ignored);
            }
        }
        if matches!(s.enc, Enc::TaskRef | Enc::NoteRef) && self.points_elsewhere(project_id, s.enc, &c.value)? {
            return Ok(Outcome::Ignored);
        }
        let local = self.local_or_placeholder(room, project_id, e)?;
        // A row moved to another board here keeps receiving nothing: it left the room.
        let on_board: bool = self.conn.query_row(
            &format!("SELECT project_id = ?2 FROM {} WHERE id = ?1", table(&e.kind)),
            params![local, project_id],
            |r| r.get(0),
        )?;
        if !on_board {
            return Ok(Outcome::Ignored);
        }
        self.write_field(room, project_id, &e.kind, local, s, &c.value)?;
        self.conn.execute(
            &format!("UPDATE {} SET updated_at = ?2, version = version + 1 WHERE id = ?1", table(&e.kind)),
            params![local, now()],
        )?;
        self.store_field(room, e, field, &c.value, &c.clock)?;
        Ok(Outcome::Applied)
    }

    fn insert_event(&self, room: &str, project_id: i64, e: &Ref, row: &Value, c: &Clock) -> Result<Outcome> {
        let get = |k: &str| row.get(k).cloned().unwrap_or(Value::Null);
        let task = self.decode_ref(room, project_id, "task", &get("task"))?;
        let note = self.decode_ref(room, project_id, "note", &get("note"))?;
        let ts = get("ts").as_i64().ok_or_else(|| Error::Other(format!("event without ts: {row}")))?;
        let text = |k: &str| get(k).as_str().map(String::from).ok_or_else(|| Error::Other(format!("event without {k}: {row}")));
        self.conn.execute(
            "INSERT INTO events (project_id, task_id, note_id, ts, actor, kind, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![project_id, task, note, ts, text("actor")?, text("kind")?, text("body")?],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO sync_ids (kind, local_id, origin, origin_id) VALUES ('event', ?1, ?2, ?3)",
            params![id, e.origin, e.id],
        )?;
        self.store_field(room, e, ROW, row, c)?;
        Ok(Outcome::Applied)
    }

    /// Whether a received reference names a row this machine has on another board: a task it
    /// moved off the shared board, or a note that went with one. Writing it would link the
    /// shared board into a private one -- the cross-board reference `task.rs` refuses -- so the
    /// change is ignored: the thing it is about left the room here. Not stored as NULL either,
    /// because that difference would flush back out and clear the reference for everyone.
    fn points_elsewhere(&self, project_id: i64, enc: Enc, v: &Value) -> Result<bool> {
        let kind = match enc {
            Enc::TaskRef => "task",
            Enc::NoteRef => "note",
            _ => return Ok(false),
        };
        let (Some(origin), Some(id)) = (v.get("origin").and_then(Value::as_str), v.get("id").and_then(Value::as_i64)) else {
            return Ok(false);
        };
        let Some(local) = self.local_id(&Ref { kind: kind.into(), origin: origin.into(), id })? else { return Ok(false) };
        let board: Option<i64> = self.conn
            .query_row(&format!("SELECT project_id FROM {} WHERE id = ?1", table(kind)), [local], |r| r.get(0))
            .optional()?;
        Ok(board.is_some_and(|b| b != project_id))
    }

    /// A task another member moved off the team board. Their history stays here, frozen and
    /// archived, rather than vanishing: it is part of this board's history too.
    fn apply_left(&self, project_id: i64, e: &Ref, author: &str) -> Result<Outcome> {
        let Some(local) = self.local_id(e)? else { return Ok(Outcome::Ignored) };
        let n = self.conn.execute(
            "UPDATE tasks SET status = 'archived', updated_at = ?3, version = version + 1 WHERE id = ?1 AND project_id = ?2",
            params![local, project_id, now()],
        )?;
        if n == 1 {
            self.write_event(project_id, Some(local), crate::core::model::Actor::System, LEFT_ROOM,
                &format!("{author} moved this off the team board"))?;
        }
        Ok(Outcome::Applied)
    }

    /// The relay's tombstone: the same removal a local forget does, then remembered so nothing
    /// about the entity can come back.
    fn apply_forget(&self, room: &str, project_id: i64, e: &Ref) -> Result<Outcome> {
        if let Some(local) = self.local_id(e)? {
            let here: bool = self.conn
                .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1 AND project_id = ?2)", table(&e.kind)), params![local, project_id], |r| r.get(0))?;
            if here {
                match e.kind.as_str() {
                    "task" => self.forget_task_unchecked(project_id, local, true)?,
                    "note" => self.forget_note_unchecked(project_id, local, true)?,
                    _ => {}
                }
            }
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO sync_forgotten (room, kind, origin, origin_id) VALUES (?1, ?2, ?3, ?4)",
            params![room, e.kind, e.origin, e.id],
        )?;
        self.conn.execute(
            "DELETE FROM sync_fields WHERE room = ?1 AND kind = ?2 AND origin = ?3 AND origin_id = ?4",
            params![room, e.kind, e.origin, e.id],
        )?;
        Ok(Outcome::Applied)
    }
}

// ---------------------------------------------------------------------------
// Handles: how tasks are named on connected boards
// ---------------------------------------------------------------------------

impl Store {
    /// Whether any board is connected. `false` on a store older than migration 014 too: the
    /// SessionStart hook reads stores it must not migrate, and naming a missing table is an
    /// error there that would silently blank the board (notes #13, #17).
    fn any_connected(&self) -> Result<bool> {
        match self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sync_rooms)", [], |r| r.get(0)) {
            Ok(b) => Ok(b),
            Err(e) if crate::core::workstream::is_missing_schema(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Handles for every task on a connected board, and the origin of each listed text that
    /// another member wrote. `texts` names them as (`"event"` | `"task"` | `"note"`, local id);
    /// for a task or note it is the body, whose origin is whoever last wrote it.
    pub fn handles(&self, texts: &[(&str, i64)]) -> Result<Handles> {
        if !self.any_connected()? {
            return Ok(Handles::default());
        }
        let tag = self.origin_tag()?;
        let mut h = Handles { tag: tag.clone(), ..Default::default() };
        // A task filed here since the last flush has no identity yet, but it will be exactly
        // (this store's tag, its local id) -- flush assigns nothing else. Naming it that way at
        // once keeps the agent that filed it from seeing (and writing down) a plain #12 first.
        let mut st = self.conn.prepare(
            "SELECT t.id, s.origin, s.origin_id FROM tasks t
               JOIN sync_rooms r ON r.project_id = t.project_id
               LEFT JOIN sync_ids s ON s.kind = 'task' AND s.local_id = t.id",
        )?;
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?)))? {
            let (local, origin, id) = row?;
            match (origin, id, &tag) {
                (Some(o), Some(i), _) => { h.tasks.insert(local, (o, i)); }
                (_, _, Some(me)) => { h.tasks.insert(local, (me.clone(), local)); }
                _ => {}
            }
        }
        let me = tag.unwrap_or_default();
        for &(kind, local) in texts {
            let origin: Option<String> = match kind {
                "event" => self.conn
                    .query_row("SELECT origin FROM sync_ids WHERE kind = 'event' AND local_id = ?1", [local], |r| r.get(0))
                    .optional()?,
                _ => {
                    let Some(r) = self.identity(kind, local)? else { continue };
                    self.conn
                        .query_row(
                            "SELECT tag FROM sync_fields WHERE kind = ?1 AND origin = ?2 AND origin_id = ?3 AND field = 'body'
                              ORDER BY wall_ms DESC, counter DESC LIMIT 1",
                            params![kind, r.origin, r.id],
                            |row| row.get(0),
                        )
                        .optional()?
                        .filter(|t: &String| !t.is_empty())
                        .or(Some(r.origin))
                }
            };
            if let Some(o) = origin.filter(|o| *o != me) {
                h.origins.insert(format!("{kind}:{local}"), o);
            }
        }
        Ok(h)
    }

    /// How a task is referred to in text this machine generates ("blocked by #12-b1"). That
    /// text reaches teammates, so it must name the task the way they can look it up.
    pub fn task_ref(&self, id: i64) -> Result<String> {
        if !self.any_connected()? {
            return Ok(format!("#{id}"));
        }
        let row: Option<(Option<String>, Option<i64>)> = self.conn
            .query_row(
                "SELECT s.origin, s.origin_id FROM tasks t
                   JOIN sync_rooms r ON r.project_id = t.project_id
                   LEFT JOIN sync_ids s ON s.kind = 'task' AND s.local_id = t.id
                  WHERE t.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(origin), Some(n))) => format!("#{n}-{origin}"),
            // On a shared board, not flushed yet: it will be this store's tag and its own id.
            Some(_) => match self.origin_tag()? {
                Some(me) => format!("#{id}-{me}"),
                None => format!("#{id}"),
            },
            None => format!("#{id}"),
        })
    }

    /// The local task an agent named: a handle (`122-v1`, `#122-v1`), or a plain number
    /// meaning a task created on this machine.
    ///
    /// A plain number that is the hidden local id of a teammate's task is refused with its
    /// handle. That number is never shown, so the only way to arrive at it is a mix-up -- most
    /// likely someone's "#40" meant their own #40 -- and resolving it would act on the wrong task.
    pub fn resolve_task_arg(&self, project_id: i64, raw: &str) -> Result<i64> {
        let s = raw.trim().trim_start_matches('#');
        if let Ok(n) = s.parse::<i64>() {
            if self.any_connected()? {
                if let Some(r) = self.identity("task", n)? {
                    if !self.is_own_tag(&r.origin)? {
                        return Err(Error::InvalidValue {
                            field: "task",
                            value: raw.trim().into(),
                            valid: format!(
                                "#{n} is not a task created on this machine. The task with local number {n} is {} -- \
                                 pass that handle if it is the one you mean",
                                self.task_ref(n)?
                            ),
                        });
                    }
                }
            }
            return Ok(n);
        }
        if let Some(h) = crate::sync::protocol::Handle::parse(s) {
            if self.any_connected()? {
                if let Some(id) = self.local_id(&Ref { kind: "task".into(), origin: h.origin.clone(), id: h.id })? {
                    self.task(project_id, id)?;
                    return Ok(id);
                }
                // This store's own handle for a task filed since the last flush: shown as
                // `#12-b1` already, so it must resolve already.
                if self.origin_tag()?.as_deref() == Some(h.origin.as_str()) && self.identity("task", h.id)?.is_none() {
                    if self.task(project_id, h.id).is_ok() && self.room_of(project_id)?.is_some() {
                        return Ok(h.id);
                    }
                }
            }
            return Err(Error::InvalidValue {
                field: "task",
                value: raw.trim().into(),
                valid: format!("a task on this board -- no task {h} is here. The board lists every task's handle"),
            });
        }
        Err(Error::InvalidValue {
            field: "task",
            value: raw.trim().into(),
            valid: "a task number like 12, or a handle like 122-v1".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Repos on shared tasks
// ---------------------------------------------------------------------------

impl Store {
    /// A task's repos as they travel: the key of every linked checkout, plus the keys kept for
    /// repos this machine has no checkout of. Sorted, so the same set is the same value.
    fn task_repo_keys(&self, task_id: i64) -> Result<Vec<String>> {
        let mut keys = std::collections::BTreeSet::new();
        let mut st = self.conn.prepare(
            "SELECT r.path, r.name FROM task_repos tr JOIN repos r ON r.id = tr.repo_id WHERE tr.task_id = ?1",
        )?;
        for row in st.query_map([task_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (path, name) = row?;
            keys.insert(crate::core::project::repo_key(&path, &name));
        }
        let mut st = self.conn.prepare("SELECT key FROM sync_unmapped_repos WHERE task_id = ?1")?;
        for k in st.query_map([task_id], |r| r.get::<_, String>(0))? {
            keys.insert(k?);
        }
        Ok(keys.into_iter().collect())
    }

    /// Links a received set of repo keys: each to the checkout on this board it names, and the
    /// rest kept as unmapped.
    fn set_task_repo_keys(&self, project_id: i64, task_id: i64, keys: &[String]) -> Result<()> {
        let mut by_key = std::collections::HashMap::new();
        for r in self.repos(project_id)? {
            by_key.insert(crate::core::project::repo_key(&r.path, &r.name), r.id);
        }
        self.conn.execute("DELETE FROM task_repos WHERE task_id = ?1", [task_id])?;
        self.conn.execute("DELETE FROM sync_unmapped_repos WHERE task_id = ?1", [task_id])?;
        for k in keys {
            match by_key.get(k) {
                Some(repo) => self.conn.execute("INSERT OR IGNORE INTO task_repos (task_id, repo_id) VALUES (?1, ?2)", params![task_id, repo])?,
                None => self.conn.execute("INSERT OR IGNORE INTO sync_unmapped_repos (task_id, key) VALUES (?1, ?2)", params![task_id, k])?,
            };
        }
        Ok(())
    }

    /// Before a repo's links are removed on this machine -- taking it off a board, forgetting
    /// it -- keeps the links of shared tasks as unmapped keys. Which checkouts this machine has
    /// is local setup; it must not take a repo off the team's tasks.
    pub(crate) fn keep_shared_repo_links(&self, repo_id: i64, project_id: Option<i64>) -> Result<()> {
        let Some((path, name)) = self.conn
            .query_row("SELECT path, name FROM repos WHERE id = ?1", [repo_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .optional()? else { return Ok(()) };
        let key = crate::core::project::repo_key(&path, &name);
        self.conn.execute(
            "INSERT OR IGNORE INTO sync_unmapped_repos (task_id, key)
             SELECT tr.task_id, ?2 FROM task_repos tr
               JOIN tasks t ON t.id = tr.task_id
               JOIN sync_rooms s ON s.project_id = t.project_id
              WHERE tr.repo_id = ?1 AND (?3 IS NULL OR t.project_id = ?3)",
            params![repo_id, key, project_id],
        )?;
        Ok(())
    }

    /// A checkout just added to a board takes over the links teammates made to the same repo
    /// while this machine had no checkout of it.
    pub(crate) fn adopt_unmapped_links(&self, project_id: i64, repo_id: i64) -> Result<()> {
        let Some((path, name)) = self.conn
            .query_row("SELECT path, name FROM repos WHERE id = ?1", [repo_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .optional()? else { return Ok(()) };
        let key = crate::core::project::repo_key(&path, &name);
        self.conn.execute(
            "INSERT OR IGNORE INTO task_repos (task_id, repo_id)
             SELECT u.task_id, ?1 FROM sync_unmapped_repos u JOIN tasks t ON t.id = u.task_id
              WHERE u.key = ?2 AND t.project_id = ?3",
            params![repo_id, key, project_id],
        )?;
        self.conn.execute(
            "DELETE FROM sync_unmapped_repos WHERE key = ?1 AND task_id IN (SELECT id FROM tasks WHERE project_id = ?2)",
            params![key, project_id],
        )?;
        Ok(())
    }

    /// Labels of the repos on a task that have no checkout here. Empty on a store too old to
    /// have the table, which the read-only hook can meet.
    pub fn unmapped_repos(&self, task_id: i64) -> Result<Vec<String>> {
        let mut st = match self.conn.prepare("SELECT key FROM sync_unmapped_repos WHERE task_id = ?1 ORDER BY key") {
            Ok(st) => st,
            Err(e) if crate::core::workstream::is_missing_schema(&e) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let keys = st.query_map([task_id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(keys.iter().map(|k| crate::core::project::repo_key_label(k).to_string()).collect())
    }
}

// ---------------------------------------------------------------------------
// What connecting and disconnecting would touch, for the CLI's summaries
// ---------------------------------------------------------------------------

/// What connecting a board publishes: its tasks, notes, and the events a room receives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublishSummary {
    pub tasks: i64,
    pub notes: i64,
    pub events: i64,
}

impl Store {
    pub fn publish_summary(&self, project_id: i64) -> Result<PublishSummary> {
        let count = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [project_id], |r| r.get(0))?) };
        let kinds = PUBLISHED_KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ");
        Ok(PublishSummary {
            tasks: count("SELECT COUNT(*) FROM tasks WHERE project_id = ?1")?,
            notes: count("SELECT COUNT(*) FROM notes WHERE project_id = ?1")?,
            events: count(&format!(
                "SELECT COUNT(*) FROM events WHERE project_id = ?1 AND (kind IN ({kinds}) OR kind LIKE 'status:%')"
            ))?,
        })
    }

    /// How many tasks on a board other members created: what a wiping disconnect removes.
    pub fn foreign_task_count(&self, project_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks t JOIN sync_ids s ON s.kind = 'task' AND s.local_id = t.id
              WHERE t.project_id = ?1 AND s.origin NOT IN (SELECT tag FROM sync_own_tags)",
            [project_id],
            |r| r.get(0),
        )?)
    }
}

// ---------------------------------------------------------------------------
// Who is on what: coordination on a shared board (docs/sync.md, phase 4)
// ---------------------------------------------------------------------------

/// Who put a task in `doing`, and when. Derived from history -- there is no assignee column,
/// which the vision rules out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The origin tag of whoever claimed it.
    pub tag: String,
    pub since: i64,
}

impl Store {
    /// Who has a task in `doing` on a shared board, earliest first. Empty unless it is in `doing`
    /// and the board is shared.
    ///
    /// Everyone whose `status:doing` came after the task last left `doing`: two people can be on
    /// one task, by joining it or by claiming in the same moment, and the one who was there first
    /// must see the second (#1068). An event's origin tag says who, or this store's tag for one
    /// not flushed yet. A task created straight into `doing` has no such event, and then its
    /// creator claimed it.
    pub fn claimers(&self, project_id: i64, task_id: i64) -> Result<Vec<Claim>> {
        if self.room_of(project_id)?.is_none() {
            return Ok(vec![]);
        }
        let task = self.task(project_id, task_id)?;
        if task.status != crate::core::model::Status::Doing {
            return Ok(vec![]);
        }
        let me = self.origin_tag()?.unwrap_or_default();
        let left: (i64, i64) = self.conn
            .query_row(
                "SELECT ts, id FROM events WHERE task_id = ?1 AND kind LIKE 'status:%' AND kind != 'status:doing'
                  ORDER BY ts DESC, id DESC LIMIT 1",
                [task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((i64::MIN, i64::MIN));
        let events: Vec<(i64, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT id, ts FROM events WHERE task_id = ?1 AND kind = 'status:doing' AND (ts, id) > (?2, ?3)
                  ORDER BY ts, id",
            )?;
            let rows = st.query_map(params![task_id, left.0, left.1], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            rows
        };
        let mut out: Vec<Claim> = Vec::new();
        for (id, ts) in events {
            let tag = self.identity("event", id)?.map(|r| r.origin).unwrap_or_else(|| me.clone());
            if !out.iter().any(|c| c.tag == tag) {
                out.push(Claim { tag, since: ts });
            }
        }
        if out.is_empty() {
            out.push(Claim { tag: self.identity("task", task_id)?.map(|r| r.origin).unwrap_or(me), since: task.created_at });
        }
        Ok(out)
    }

    pub fn claims(&self, project_id: i64) -> Result<Vec<(i64, Claim)>> {
        if self.room_of(project_id)?.is_none() {
            return Ok(vec![]);
        }
        let ids: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM tasks WHERE project_id = ?1 AND status = 'doing' ORDER BY id")?;
            let ids = st.query_map([project_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            ids
        };
        let mut out = Vec::new();
        for id in ids {
            out.extend(self.claimers(project_id, id)?.into_iter().map(|c| (id, c)));
        }
        Ok(out)
    }

    /// Open tasks on a shared board whose titles say what `task_id`'s says: most of its
    /// significant words, and at least two of them. Best match first, at most `limit`.
    ///
    /// Words rather than FTS ranking: bm25 has no threshold that means "the same task", and a
    /// hint that fires on every filing is one the agent learns to skip. Only on shared boards,
    /// where the problem is two people solving one thing twice; a private board renders as ever.
    pub fn possible_duplicates(&self, project_id: i64, task_id: i64, limit: usize) -> Result<Vec<crate::core::model::Task>> {
        if self.room_of(project_id)?.is_none() {
            return Ok(vec![]);
        }
        let new = self.task(project_id, task_id)?;
        let wanted = significant_words(&new.title);
        if wanted.len() < 2 {
            return Ok(vec![]);
        }
        let open = self.board(project_id, &crate::core::model::BoardQuery::board().with_limit(usize::MAX))?.tasks;
        let mut scored: Vec<(usize, crate::core::model::Task)> = open
            .into_iter()
            .filter(|t| t.id != task_id)
            .filter_map(|t| {
                let shared = significant_words(&t.title).intersection(&wanted).count();
                (shared >= 2 && shared * 2 >= wanted.len()).then_some((shared, t))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)).then(b.1.id.cmp(&a.1.id)));
        Ok(scored.into_iter().take(limit).map(|(_, t)| t).collect())
    }
}

/// The words of a title that say what it is about: lowercased, four letters or more, minus the
/// filler that would make every pair of titles look alike.
fn significant_words(title: &str) -> std::collections::BTreeSet<String> {
    const FILLER: &[&str] = &["with", "from", "that", "this", "when", "into", "after", "before", "should", "does", "doesn", "have", "make", "still", "again", "some"];
    title
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= 4 && !FILLER.contains(&w.as_str()))
        .collect()
}
