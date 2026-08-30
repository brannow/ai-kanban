//! Paged reads, for the consumer that scrolls.
//!
//! # Why these exist next to `board()` rather than replacing it
//!
//! `board()` is capped and reports the remainder as a count, because an agent pays for every
//! token and an expensive board is one it routes around. A person scrolling a column wants
//! the opposite: everything, a page at a time. Two consumers, two shapes, one core.
//!
//! Widening `BoardQuery`'s caps to serve a web page instead would leave `tests/budget.rs`
//! asserting the web UI's page size rather than the agent's token budget -- which `CLAUDE.md`
//! already names as the way the board quietly becomes too expensive to use.
//!
//! # Keyset, not `LIMIT`/`OFFSET`
//!
//! An agent is writing to this store while a person scrolls it. Under `OFFSET`, a row
//! inserted above the window shifts everything down, so page 2 repeats a row page 1 already
//! showed and skips one entirely -- silently, and only under concurrency, which is the worst
//! way to find a bug. Keyset asks for "the rows after this exact position" and is immune.
//!
//! The position is `(timestamp, id)` rather than the timestamp alone, for the reason stated
//! everywhere else in this codebase: timestamps are whole seconds, so rows written in the
//! same second tie, and a tie at a page boundary loses or repeats rows.

use crate::core::error::Result;
use crate::core::event::housekeeping_filter;
use crate::core::model::{Event, Note, Status, Task};
use crate::core::note::split_tags;
use crate::core::store::Store;
use crate::core::task::{row_to_task, TASK_COLS};
use rusqlite::params;

/// A position in an ordered listing: the sort key of the last row returned.
///
/// Deliberately opaque to clients -- the HTTP layer encodes it as a string the caller echoes
/// back and never parses. Otherwise the first change to the sort order becomes a breaking
/// API change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub ts: i64,
    pub id: i64,
}

/// One page, plus where to resume. `next` is `None` at the end of the listing.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    /// Fetches `limit + 1` and keeps `limit`, so "is there more" is answered by the query
    /// rather than by a second `COUNT(*)` that could disagree with it under a concurrent write.
    fn build(mut items: Vec<T>, limit: usize, key: impl Fn(&T) -> Cursor) -> Self {
        let next = if items.len() > limit {
            items.truncate(limit);
            items.last().map(&key)
        } else {
            None
        };
        Page { items, next }
    }
}

/// `WHERE` fragment for "strictly after this position" in a `DESC` listing. Written out
/// rather than as a row-value comparison so the index is used on every SQLite build.
///
/// **The parameter numbers are fixed and every caller must bind in the same order**:
/// `?1` project, `?2` limit, `?3` cursor timestamp, `?4` cursor id. `params![]` binds
/// positionally, so a placeholder number here that no argument lines up with is simply never
/// bound -- which fails at execution rather than at compile time, and only on the cursor
/// path, which is the second page nobody tests by hand.
fn after(col: &str, cursor: Option<Cursor>) -> String {
    match cursor {
        Some(_) => format!(" AND ({col} < ?3 OR ({col} = ?3 AND id < ?4))"),
        None => String::new(),
    }
}

impl Store {
    /// Tasks in the given statuses, newest first. Empty `statuses` means every status --
    /// unlike `board()`, where empty means "the open ones", because a person filtering a
    /// column has said exactly what they want.
    pub fn tasks_page(
        &self, project_id: i64, statuses: &[Status], cursor: Option<Cursor>, limit: usize,
    ) -> Result<Page<Task>> {
        let filter = if statuses.is_empty() {
            String::new()
        } else {
            let list = statuses.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
            format!(" AND status IN ({list})")
        };
        let sql = format!(
            "SELECT {TASK_COLS} FROM tasks WHERE project_id = ?1{filter}{}
             ORDER BY updated_at DESC, id DESC LIMIT ?2",
            after("updated_at", cursor)
        );
        let mut st = self.conn.prepare(&sql)?;
        let over = limit as i64 + 1;
        let items: Vec<Task> = match cursor {
            Some(c) => st.query_map(params![project_id, over, c.ts, c.id], row_to_task)?,
            None => st.query_map(params![project_id, over], row_to_task)?,
        }
        .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Page::build(items, limit, |t| Cursor { ts: t.updated_at, id: t.id }))
    }

    /// Project history, newest first.
    ///
    /// Housekeeping kinds are excluded here as they are everywhere else: a timeline made of
    /// "learned path .../src/core" is not history a person reads either. A project's paths
    /// are shown from `project_paths`, which is the actual data rather than a record of it
    /// being written.
    pub fn events_page(&self, project_id: i64, cursor: Option<Cursor>, limit: usize) -> Result<Page<Event>> {
        let sql = format!(
            "SELECT id, project_id, task_id, ts, actor, kind, body FROM events
              WHERE project_id = ?1 AND {}{}
              ORDER BY ts DESC, id DESC LIMIT ?2",
            housekeeping_filter(),
            after("ts", cursor)
        );
        let mut st = self.conn.prepare(&sql)?;
        let over = limit as i64 + 1;
        let row = |r: &rusqlite::Row<'_>| {
            Ok(Event {
                id: r.get(0)?, project_id: r.get(1)?, task_id: r.get(2)?,
                ts: r.get(3)?, actor: r.get(4)?, kind: r.get(5)?, body: r.get(6)?,
            })
        };
        let items: Vec<Event> = match cursor {
            Some(c) => st.query_map(params![project_id, over, c.ts, c.id], row)?,
            None => st.query_map(params![project_id, over], row)?,
        }
        .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Page::build(items, limit, |e| Cursor { ts: e.ts, id: e.id }))
    }

    /// Every note on a board, newest first. The knowledge layer had no project-wide listing
    /// before this: an agent reaches notes through `recall` or through the file it is
    /// editing, never as a list, because a list of everything is not an answer to anything.
    /// A person browsing their own memory wants exactly that list.
    pub fn notes_page(&self, project_id: i64, cursor: Option<Cursor>, limit: usize) -> Result<Page<Note>> {
        let sql = format!(
            "SELECT id, project_id, task_id, title, body, tags, created_at, updated_at, version
               FROM notes WHERE project_id = ?1{}
              ORDER BY updated_at DESC, id DESC LIMIT ?2",
            after("updated_at", cursor)
        );
        let mut st = self.conn.prepare(&sql)?;
        let over = limit as i64 + 1;
        let row = |r: &rusqlite::Row<'_>| {
            let tags: String = r.get(5)?;
            Ok(Note {
                id: r.get(0)?, project_id: r.get(1)?, task_id: r.get(2)?,
                title: r.get(3)?, body: r.get(4)?, tags: split_tags(&tags),
                paths: Vec::new(),
                created_at: r.get(6)?, updated_at: r.get(7)?, version: r.get(8)?,
            })
        };
        let mut items: Vec<Note> = match cursor {
            Some(c) => st.query_map(params![project_id, over, c.ts, c.id], row)?,
            None => st.query_map(params![project_id, over], row)?,
        }
        .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(st);
        for n in &mut items {
            n.paths = self.note_paths(n.id)?;
        }

        Ok(Page::build(items, limit, |n| Cursor { ts: n.updated_at, id: n.id }))
    }

    /// The change cursor: the highest event id in the store.
    ///
    /// This is what the live stream polls. It is a valid cursor only because every mutation
    /// writes an event (`tests/change_feed.rs`) and because event ids are `AUTOINCREMENT`, so
    /// a delete cannot hand the next insert an id that was already used -- see migration 004.
    ///
    /// Deliberately store-wide rather than per project: one poll serves every connected
    /// client whatever board they are watching, and per-project filtering happens when the
    /// change is dispatched.
    pub fn change_cursor(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?)
    }

    /// Which projects changed since `since`, so a poll can tell one client from another.
    pub fn projects_changed_since(&self, since: i64) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT project_id FROM events WHERE id > ?1",
        )?;
        Ok(st.query_map([since], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
