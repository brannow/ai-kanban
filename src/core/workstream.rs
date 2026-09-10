//! Workstreams: the relevance dimension the board was missing.
//!
//! # The problem this solves
//!
//! `board()` had two selection knobs, status and limit, and ordered by recency. That is a
//! fair model for one repo with one stream of work. On a site maintained for years, with
//! several features in flight, it breaks in a way that is worse than noise: measured on
//! 300 tasks across 12 workstreams, the 30 rows the board listed came from 4 workstreams
//! picked purely by recency, and the workstream actually being worked on contributed 1.
//! A cold agent was told, confidently and in detail, about work that was not its own.
//!
//! # Why a table and not tags
//!
//! The directory line needs `name, COUNT(*)` per workstream on every board call. `tags` is
//! a comma-joined TEXT column, and nothing in this codebase enumerates it -- answering that
//! from joined text means splitting every row in the project. Tags stay what they are: a
//! freeform, multi-valued, human-facing label. A workstream is single-valued, enumerable,
//! and closable. Different shape, different job.
//!
//! # Why the agent never manages one
//!
//! There is no `workstream_create` or `workstream_set` tool, because "manage a workstream"
//! is not an intent an agent has -- it is bookkeeping, and `docs/tool-design.md` maps tools
//! to intents. Workstreams are created and entered as a *side effect* of the agent doing
//! what it was already doing: asking to see a slice of the board. The precedent is
//! `add_path_alias`, which learns a path as a side effect of using a board rather than
//! through a call of its own.

use crate::core::error::Result;
use crate::core::model::{Actor, Workstream, WorkstreamSummary};
use crate::core::store::{now, Store};
use rusqlite::{params, OptionalExtension};

/// Collapses the spellings of one workstream into a single name.
///
/// The realistic failure is not malice, it is an agent writing `contact-form` in one
/// session and `Contact Form` in the next, leaving a board with two half-workstreams. That
/// is the same split-memory shape `normalize_remote` exists to prevent for projects, one
/// level down -- and it is cheap to prevent here because the UNIQUE constraint then
/// actually catches the near-miss instead of storing it as a second row.
///
/// Deliberately lossy and deliberately dumb: no stemming, no fuzzy matching. A rule an
/// agent cannot predict is worse than one that occasionally keeps two names apart.
pub fn normalize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_sep = false;
    for ch in raw.trim().chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_sep = false;
        } else if !prev_sep && !out.is_empty() {
            // Any run of separators -- spaces, underscores, slashes -- becomes one hyphen.
            out.push('-');
            prev_sep = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    out
}

fn row_to_workstream(r: &rusqlite::Row<'_>) -> rusqlite::Result<Workstream> {
    Ok(Workstream {
        id: r.get(0)?, project_id: r.get(1)?, name: r.get(2)?,
        created_at: r.get(3)?, closed_at: r.get(4)?,
    })
}

const WS_COLS: &str = "id, project_id, name, created_at, closed_at";

/// `WS_COLS` with a table alias, for the queries that join. Derived rather than written out
/// a second time: two hand-maintained column lists feeding the same `row_to_workstream` is
/// how a column added to one and not the other shifts every index after it. That trap has
/// already been paid for once here -- see the `TASK_COLS` note in CLAUDE.md.
fn ws_cols(alias: &str) -> String {
    WS_COLS.split(", ").map(|c| format!("{alias}.{c}")).collect::<Vec<_>>().join(", ")
}

/// True when a query failed only because this store predates migration 005.
///
/// The read paths below run from the SessionStart hook, which opens the store **read-only**
/// and must never migrate it. On a store the binary has not upgraded yet, the workstream
/// tables simply do not exist. That has to degrade to "this board has no workstreams" --
/// which renders exactly the pre-005 board -- rather than to an error that takes the whole
/// hook output down with it. Matching on the message is crude; it is also the only thing
/// SQLite gives us that distinguishes a missing table from a real failure.
pub(crate) fn is_missing_schema(e: &rusqlite::Error) -> bool {
    let m = e.to_string();
    m.contains("no such table") || m.contains("no such column")
}

impl Store {
    /// The workstream a board is currently scoped to, if any.
    ///
    /// Tolerant by design -- see `is_missing_schema`. Returns `None` both when no
    /// workstream is set and when the store is too old to have any.
    pub fn current_workstream(&self, project_id: i64) -> Result<Option<Workstream>> {
        let r = self.conn.query_row(
            &format!(
                "SELECT {} FROM workstreams w
                   JOIN current_workstream c ON c.workstream_id = w.id
                  WHERE c.project_id = ?1",
                ws_cols("w"),
            ),
            [project_id],
            row_to_workstream,
        ).optional();
        match r {
            Ok(v) => Ok(v),
            Err(e) if is_missing_schema(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn workstream_by_name(&self, project_id: i64, name: &str) -> Result<Option<Workstream>> {
        let norm = normalize_name(name);
        let r = self.conn.query_row(
            &format!("SELECT {WS_COLS} FROM workstreams WHERE project_id = ?1 AND name = ?2"),
            params![project_id, norm],
            row_to_workstream,
        ).optional();
        match r {
            Ok(v) => Ok(v),
            Err(e) if is_missing_schema(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Find or create. Creating writes an event, because starting a workstream is real
    /// project history -- "we began the contact-form work" is exactly the kind of thing a
    /// cold agent benefits from seeing in `recent`.
    pub fn ensure_workstream(&self, project_id: i64, name: &str) -> Result<Workstream> {
        let norm = normalize_name(name);
        if norm.is_empty() {
            return Err(crate::core::Error::Other("a workstream needs a name".into()));
        }
        if let Some(w) = self.workstream_by_name(project_id, &norm)? {
            return Ok(w);
        }
        self.conn.execute(
            "INSERT INTO workstreams (project_id, name, created_at) VALUES (?1, ?2, ?3)",
            params![project_id, norm, now()],
        )?;
        let w = self.workstream_by_name(project_id, &norm)?.expect("just inserted");
        self.write_event(project_id, None, Actor::Agent, "workstream_created", &w.name)?;
        Ok(w)
    }

    /// Point the board at a workstream. Idempotent.
    ///
    /// Writes a housekeeping event: it is a change the HTTP live stream has to see (it
    /// polls `MAX(events.id)`), but it is not news for the agent's `recent` -- switching
    /// scope is navigation, not project history.
    pub fn set_current_workstream(&self, project_id: i64, workstream_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO current_workstream (project_id, workstream_id, set_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(project_id) DO UPDATE SET workstream_id = ?2, set_at = ?3",
            params![project_id, workstream_id, now()],
        )?;
        self.write_event(project_id, None, Actor::Agent, "workstream_entered", &self.workstream(project_id, workstream_id)?.name)?;
        Ok(())
    }

    /// Widen back out to the whole board. Idempotent, and silent when nothing was set --
    /// leaving a scope you are not in is not an error, and making it one would force the
    /// agent to track state it should not have to know about.
    pub fn clear_current_workstream(&self, project_id: i64) -> Result<()> {
        let removed = self.conn.execute(
            "DELETE FROM current_workstream WHERE project_id = ?1", [project_id],
        )?;
        if removed > 0 {
            self.write_event(project_id, None, Actor::Agent, "workstream_entered", "")?;
        }
        Ok(())
    }

    pub fn workstream(&self, project_id: i64, id: i64) -> Result<Workstream> {
        // Scoped by project for the same reason task lookups are: the store is global, and
        // a bare id could read a row belonging to another board.
        self.conn.query_row(
            &format!("SELECT {WS_COLS} FROM workstreams WHERE id = ?1 AND project_id = ?2"),
            params![id, project_id],
            row_to_workstream,
        ).optional()?.ok_or_else(|| crate::core::Error::Other(format!("no workstream #{id} on this board")))
    }

    /// The directory: every open workstream with unfinished work, most recently active
    /// first, optionally excluding the one already being shown in full.
    ///
    /// Closed workstreams drop out here and nowhere else -- their tasks stay on the board
    /// and stay counted. Closing is about keeping the directory readable after three years,
    /// not about hiding work.
    pub fn workstream_summaries(&self, project_id: i64, exclude: Option<i64>) -> Result<Vec<WorkstreamSummary>> {
        self.summaries(project_id, exclude, true)
    }

    /// Every open workstream, **including ones with no open tasks**.
    ///
    /// The difference from `workstream_summaries` is not a detail. The agent's directory
    /// hides empty workstreams because a line reading "contact-form 0" is noise in a
    /// response with a token budget. A human's selector must show them, or a workstream
    /// created a moment ago is one nobody can pick -- which would make the control quietly
    /// lossy in exactly the way this feature exists to prevent.
    pub fn all_open_workstreams(&self, project_id: i64) -> Result<Vec<WorkstreamSummary>> {
        self.summaries(project_id, None, false)
    }

    fn summaries(&self, project_id: i64, exclude: Option<i64>, require_open: bool) -> Result<Vec<WorkstreamSummary>> {
        let having = if require_open { "HAVING COUNT(t.id) > 0" } else { "" };
        let sql = format!(
            "SELECT {}, COUNT(t.id)
               FROM workstreams w
               LEFT JOIN tasks t
                 ON t.workstream_id = w.id AND t.status IN ('backlog','doing','blocked')
              WHERE w.project_id = ?1 AND w.closed_at IS NULL AND (?2 IS NULL OR w.id != ?2)
              GROUP BY w.id
              {having}
              ORDER BY COUNT(t.id) DESC, w.id DESC",
            ws_cols("w"),
        );
        let r = (|| -> rusqlite::Result<Vec<WorkstreamSummary>> {
            let mut st = self.conn.prepare(&sql)?;
            let rows = st.query_map(params![project_id, exclude], |r| {
                Ok(WorkstreamSummary { workstream: row_to_workstream(r)?, open: r.get::<_, i64>(5)? as usize })
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })();
        match r {
            Ok(v) => Ok(v),
            Err(e) if is_missing_schema(&e) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// Close a workstream. Its tasks are untouched; only the directory changes.
    pub fn close_workstream(&self, project_id: i64, id: i64) -> Result<Workstream> {
        let w = self.workstream(project_id, id)?;
        self.conn.execute(
            "UPDATE workstreams SET closed_at = ?3 WHERE id = ?1 AND project_id = ?2",
            params![id, project_id, now()],
        )?;
        // Clearing the pointer matters: a closed workstream left as the current scope would
        // keep filtering the board to work that is finished, with nothing saying why.
        self.conn.execute(
            "DELETE FROM current_workstream WHERE project_id = ?1 AND workstream_id = ?2",
            params![project_id, id],
        )?;
        self.write_event(project_id, None, Actor::Agent, "workstream_closed", &w.name)?;
        self.workstream(project_id, id)
    }
}
