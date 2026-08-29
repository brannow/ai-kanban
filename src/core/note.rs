//! Durable knowledge about the code. Notes have no lifecycle and outlive the task that
//! produced them -- that is the whole reason they are a separate entity from events.
//! Without them, knowledge is time-stamped narrative where a superseded fact sits next to
//! the current one with nothing marking which is which.

use crate::core::error::{Error, Result};
use crate::core::model::{Actor, Note};
use crate::core::store::{now, Store};
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct NoteDraft {
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    /// The files this note is about. NOTHING READS THIS IN V1 -- it ships because the form
    /// of memory that actually gets used is contextual ("you're editing auth.rs, here's
    /// what you learned about it"), and retrofitting it means re-tagging every note ever
    /// written.
    pub paths: Vec<String>,
    pub task_id: Option<i64>,
}

impl NoteDraft {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), body: String::new(), tags: vec![], paths: vec![], task_id: None }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NotePatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub actor: Actor,
    /// See `TaskPatch::expected_version` -- same contract, same reason it is optional.
    pub expected_version: Option<i64>,
}

const NOTE_COLS: &str = "id, project_id, task_id, title, body, tags, created_at, updated_at, version";

fn row_to_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    let tags: String = r.get(5)?;
    Ok(Note {
        id: r.get(0)?,
        project_id: r.get(1)?,
        task_id: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        tags: split_tags(&tags),
        paths: Vec::new(), // filled by hydrate_paths
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        version: r.get(8)?,
    })
}

pub(crate) fn split_tags(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

fn join_tags(tags: &[String]) -> String {
    tags.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(",")
}

impl Store {
    pub fn create_note(&self, project_id: i64, draft: NoteDraft, actor: Actor) -> Result<Note> {
        // A note attached to another board's task would surface in that board's task_show
        // while being counted in this one's history.
        if let Some(tid) = draft.task_id {
            self.ensure_task_in_project(project_id, tid)?;
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO notes (project_id, task_id, title, body, tags, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![project_id, draft.task_id, draft.title, draft.body, join_tags(&draft.tags), ts],
        )?;
        let id = self.conn.last_insert_rowid();
        self.set_note_paths(id, &draft.paths)?;
        self.write_event_for(project_id, draft.task_id, Some(id), actor, "note_added", &draft.title)?;
        self.note(project_id, id)
    }

    /// Guards every cross-entity reference. The store is global, so an id alone never
    /// proves which board it belongs to.
    pub(crate) fn ensure_task_in_project(&self, project_id: i64, task_id: i64) -> Result<()> {
        if self.task_opt(project_id, task_id)?.is_none() {
            return Err(Error::Other(format!(
                "task #{task_id} is not on this board, so nothing can be attached to it"
            )));
        }
        Ok(())
    }

    /// Updates a note **and records that it changed**.
    ///
    /// `notes` holds current state only. An in-place update with no trace would destroy the
    /// record that a fact changed -- and on a board whose first priority is history, the
    /// knowledge layer is the worst place to lose it. So every update writes an event
    /// carrying the previous value. Current state stays queryable; the change stays in
    /// history with everything else. Costs one insert.
    pub fn update_note(&self, project_id: i64, id: i64, patch: NotePatch) -> Result<Note> {
        let before = self.note(project_id, id)?;
        let title = patch.title.clone().unwrap_or_else(|| before.title.clone());
        let body = patch.body.clone().unwrap_or_else(|| before.body.clone());
        let tags = patch.tags.clone().unwrap_or_else(|| before.tags.clone());

        let changed = self.conn.execute(
            "UPDATE notes SET title=?2, body=?3, tags=?4, updated_at=?5, version = version + 1
              WHERE id=?1 AND (?6 IS NULL OR version = ?6)",
            params![id, title, body, join_tags(&tags), now(), patch.expected_version],
        )?;
        if changed == 0 {
            let actual = self.note(project_id, id)?.version;
            return Err(Error::Conflict { id, expected: patch.expected_version.unwrap_or(0), actual });
        }
        if let Some(paths) = &patch.paths {
            self.set_note_paths(id, paths)?;
        }

        let previous = if before.body.is_empty() { before.title.clone() } else { before.body.clone() };
        // `Some(id)` is what makes this removable later: the body below embeds the note's
        // previous content, so without the reference it is text nothing can ever find again.
        self.write_event_for(
            before.project_id, before.task_id, Some(id), patch.actor, "note_updated",
            &format!("note #{id} \"{}\" -- was: {}", before.title, truncate(&previous, 400)),
        )?;
        self.note(project_id, id)
    }

    /// Scoped to a project for the same reason tasks are: the store is global, and a bare
    /// id lookup would let an edit land on another board while the response names this one.
    /// `recall(project: "all")` renders note ids from other boards, so this is a path an
    /// agent can reach by following its own search results.
    pub fn note(&self, project_id: i64, id: i64) -> Result<Note> {
        let mut note = self.conn
            .query_row(
                &format!("SELECT {NOTE_COLS} FROM notes WHERE id = ?1 AND project_id = ?2"),
                params![id, project_id],
                row_to_note,
            )
            .optional()?
            .ok_or_else(|| Error::NoteNotFound {
                id,
                project: self.project(project_id).map(|p| p.name).unwrap_or_default(),
            })?;
        note.paths = self.note_paths(id)?;
        Ok(note)
    }

    pub fn notes_for_task(&self, project_id: i64, task_id: i64) -> Result<Vec<Note>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {NOTE_COLS} FROM notes WHERE task_id = ?1 AND project_id = ?2 ORDER BY updated_at DESC, id DESC"
        ))?;
        let mut notes = st.query_map(params![task_id, project_id], row_to_note)?.collect::<rusqlite::Result<Vec<_>>>()?;
        for n in &mut notes { n.paths = self.note_paths(n.id)?; }
        Ok(notes)
    }

    pub fn note_paths(&self, note_id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM note_paths WHERE note_id = ?1 ORDER BY path")?;
        Ok(st.query_map([note_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn set_note_paths(&self, note_id: i64, paths: &[String]) -> Result<()> {
        self.conn.execute("DELETE FROM note_paths WHERE note_id = ?1", [note_id])?;
        for p in paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            self.conn.execute(
                "INSERT OR IGNORE INTO note_paths (note_id, path) VALUES (?1, ?2)",
                params![note_id, p],
            )?;
        }
        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}...")
}

impl Store {
    /// Notes associated with a file, newest first.
    ///
    /// This is the query `note_paths` was created for and shipped without: proactive
    /// contextual recall. The agent opens `auth/middleware.rs` and what was learned about
    /// it last time arrives without anyone asking, because the alternative -- the agent
    /// deciding to search its own memory -- does not happen. It hits a bug and starts
    /// debugging.
    ///
    /// # Matching
    ///
    /// The caller has an absolute path; the stored path is whatever the agent wrote, which
    /// is usually repo-relative. So a stored path matches when it *is* the absolute path, or
    /// is a suffix of it **at a directory boundary**.
    ///
    /// That boundary is the whole difference between useful and useless. A plain suffix
    /// match means a note filed against `auth.rs` fires for `vendor/other/auth.rs`, and a
    /// note that surfaces on the wrong file is worse than no note at all -- it is a
    /// confident claim about code it was never about.
    ///
    /// The stored path is escaped before it becomes a `LIKE` pattern. `_` is LIKE's
    /// single-character wildcard, and underscores in filenames are ordinary -- unescaped,
    /// a note about `auth_guard.rs` would also fire on `authXguard.rs`.
    pub fn notes_for_path(&self, project_id: i64, abs_path: &str) -> Result<Vec<Note>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT DISTINCT {}
               FROM notes n JOIN note_paths np ON np.note_id = n.id
              WHERE n.project_id = ?1
                AND (?2 = np.path
                     OR ?2 LIKE '%/' || replace(replace(replace(np.path, '\\', '\\\\'), '%', '\\%'), '_', '\\_') ESCAPE '\\')
              ORDER BY n.updated_at DESC, n.id DESC",
            NOTE_COLS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ")
        ))?;
        let mut notes = st.query_map(params![project_id, abs_path], row_to_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for n in &mut notes {
            n.paths = self.note_paths(n.id)?;
        }
        Ok(notes)
    }
}
