//! Append-only history.
//!
//! A `task_id` of NULL means project-level history -- a decision, a session summary --
//! that needs no task to hang off. Its reader is the board snapshot's `recent` section.
//! Naming the reader matters: an append-only log nobody reads is exactly the extra work
//! an agent correctly skips, and it also bounds how much narrative belongs in one entry.

use crate::core::error::Result;
use crate::core::model::{Actor, Event, Project};
use crate::core::store::{now, Store};
use rusqlite::params;

const EVENT_COLS: &str = "id, project_id, task_id, ts, actor, kind, body";

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        project_id: r.get(1)?,
        task_id: r.get(2)?,
        ts: r.get(3)?,
        actor: r.get(4)?,
        kind: r.get(5)?,
        body: r.get(6)?,
    })
}

/// Events that record how the **store** changed, not how the **work** changed.
///
/// They are written because the events table is the change feed the HTTP live stream polls
/// (`docs/http-api.md`): a mutation that writes no event is invisible to it. They are kept
/// out of the agent's `recent` section because that is eight lines of the most expensive
/// space in the product, and "learned path /Users/x/repo/src/core" is not what an agent
/// asking "what happened here lately" means.
///
/// `path_learned` in particular fires the first time a board is used from any new
/// subdirectory, so left unfiltered it would both bury real history and -- through
/// `last_activity` -- make an untouched project report as recently active.
///
/// `workstream_entered` is here and `workstream_created` deliberately is not: starting a
/// piece of work is history a cold agent benefits from ("we began the contact-form work"),
/// whereas switching which slice you are looking at is navigation, not news.
///
/// `profile_allowed` / `profile_denied` are which buttons the board page offers a person, and
/// say nothing about the work.
pub(crate) const HOUSEKEEPING_KINDS: &[&str] =
    &["path_learned", "workstream_entered", "profile_allowed", "profile_denied"];

/// Events whose body is a copy of the title of the thing they happened to. Recall excludes
/// them, or every task and note matches twice: once as itself, once as an event repeating
/// its own name.
///
/// Shared rather than written out at each use for the same reason as the list above: recall
/// both SEARCHES events and COUNTS them to report what its cap left out, and a filter that
/// drifted between those two would count rows the search can never return -- reporting
/// omissions that do not exist.
pub(crate) const DUPLICATE_TITLE_KINDS: &[&str] = &["created", "note_added"];

pub(crate) fn duplicate_title_filter() -> String {
    let list = DUPLICATE_TITLE_KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ");
    format!("kind NOT IN ({list})")
}

/// Built from the list above rather than written out, so the two cannot drift. The values
/// are compile-time constants, never caller input, so inlining them is safe.
pub(crate) fn housekeeping_filter() -> String {
    let list = HOUSEKEEPING_KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ");
    format!("kind NOT IN ({list})")
}

impl Store {
    pub fn write_event(&self, project_id: i64, task_id: Option<i64>, actor: Actor, kind: &str, body: &str) -> Result<i64> {
        self.write_event_for(project_id, task_id, None, actor, kind, body)
    }

    /// `write_event` plus the note the event is about.
    ///
    /// Only note events pass a `note_id`, and they must: it is how `forget_note` finds the
    /// history to remove. A `note_updated` body embeds the note's previous body verbatim, so
    /// an event with no note reference is content that can never be located again, let alone
    /// deleted. See migration 003.
    pub(crate) fn write_event_for(
        &self, project_id: i64, task_id: Option<i64>, note_id: Option<i64>,
        actor: Actor, kind: &str, body: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (project_id, task_id, note_id, ts, actor, kind, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![project_id, task_id, note_id, now(), actor, kind, body],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The `log` tool's intent: "record what happened / what we decided", no task needed.
    pub fn log(&self, project_id: i64, actor: Actor, body: &str) -> Result<i64> {
        self.write_event(project_id, None, actor, "log", body)
    }

    /// `log` with an optional task reference, ownership checked. Unchecked, an event could
    /// carry this project's `project_id` and another board's `task_id` -- appearing in that
    /// board's task history while being counted in this one's recent section.
    pub fn log_on(&self, project_id: i64, task_id: Option<i64>, actor: Actor, body: &str) -> Result<i64> {
        if let Some(tid) = task_id {
            self.ensure_task_in_project(project_id, tid)?;
        }
        self.write_event(project_id, task_id, actor, "log", body)
    }

    /// Oldest first -- reading a task's history is reading a story, and stories run forward.
    pub fn task_events(&self, task_id: i64) -> Result<Vec<Event>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events WHERE task_id = ?1 ORDER BY ts ASC, id ASC"
        ))?;
        Ok(st.query_map([task_id], row_to_event)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Newest first, capped -- this feeds the board's `recent` section, where the point is
    /// "what changed lately", not the whole story.
    pub fn recent_events(&self, project_id: i64, limit: usize) -> Result<Vec<Event>> {
        self.recent_events_in(project_id, limit, None)
    }

    /// `recent_events` restricted to a workstream.
    ///
    /// Scoping this matters as much as scoping the task list, and it was measured
    /// separately: on the 300-task board of note #16, all eight `recent` entries came from
    /// one workstream nobody was working on. A `recent` section that reports somebody
    /// else's activity is worse than none -- it is the section an agent reads to answer
    /// "what has been happening here", and it was answering about the wrong work.
    ///
    /// Two kinds of event survive any scope, and both deliberately:
    ///
    /// * **Project-level events** (`task_id IS NULL`) -- a decision, a session summary, a
    ///   `log`. They were never about one workstream, and they are the entries most worth
    ///   carrying across a scope change.
    /// * **Events on unscoped tasks**, for the same reason unscoped tasks stay on the
    ///   board: general project work belongs to every view.
    pub fn recent_events_in(&self, project_id: i64, limit: usize, workstream: Option<i64>) -> Result<Vec<Event>> {
        // LEFT JOIN, not JOIN: an event whose task has since been deleted still belongs in
        // the history, and an inner join would silently drop it.
        // Unscoped takes the plain query, which never names `workstream_id`. On a store
        // this binary has not migrated the column does not exist, and naming it anywhere --
        // WHERE included -- fails the query and silently costs the hook its board.
        let Some(ws) = workstream else {
            let sql = format!(
                "SELECT {EVENT_COLS} FROM events WHERE project_id = ?1 AND {}
                  ORDER BY ts DESC, id DESC LIMIT ?2",
                housekeeping_filter()
            );
            let mut st = self.conn.prepare(&sql)?;
            return Ok(st.query_map(params![project_id, limit as i64], row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?);
        };
        let sql = format!(
            "SELECT {} FROM events e
               LEFT JOIN tasks t ON t.id = e.task_id
              WHERE e.project_id = ?1 AND {}
                AND (e.task_id IS NULL OR t.workstream_id = ?3 OR t.workstream_id IS NULL)
              ORDER BY e.ts DESC, e.id DESC LIMIT ?2",
            EVENT_COLS.split(", ").map(|c| format!("e.{c}")).collect::<Vec<_>>().join(", "),
            housekeeping_filter().replace("kind ", "e.kind "),
        );
        let r = (|| -> rusqlite::Result<Vec<Event>> {
            let mut st = self.conn.prepare(&sql)?;
            let rows = st.query_map(params![project_id, limit as i64, ws], row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })();
        match r {
            Ok(v) => Ok(v),
            // Same tolerance as the rest of the workstream reads: a store this binary has
            // not migrated has no `tasks.workstream_id`, and the hook must still render.
            Err(e) if e.to_string().contains("no such column") => self.recent_events(project_id, limit),
            Err(e) => Err(e.into()),
        }
    }

    /// Drives "last active 3d ago" in project summaries, so it asks about *work*. Counting
    /// housekeeping here would make a project an agent merely walked through look busy.
    pub fn last_activity(&self, project_id: i64) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            &format!("SELECT MAX(ts) FROM events WHERE project_id = ?1 AND {}", housekeeping_filter()),
            [project_id],
            |r| r.get::<_, Option<i64>>(0),
        )?)
    }

    pub fn project(&self, id: i64) -> Result<Project> {
        Ok(self.conn.query_row(
            "SELECT id, key, name, created_at FROM projects WHERE id = ?1",
            [id],
            |r| Ok(Project { id: r.get(0)?, key: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? }),
        )?)
    }
}

/// Events whose `kind` is a status transition encode the new status as `status:<value>`.
/// Exposed as a helper so every renderer reads it the same way rather than each inventing
/// its own string handling.
pub fn status_transition(kind: &str) -> Option<&str> {
    kind.strip_prefix("status:")
}
