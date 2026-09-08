//! Getting the store out of the store: `backup`, `export`, `import`.
//!
//! Everything of value here is one SQLite file outside version control, and until this
//! module existed the advice for keeping it was "copy the file". Under WAL that is wrong
//! and wrong *silently* -- recent work lives in a `-wal` sidecar, so a plain copy restores
//! a board that is days stale with nothing to indicate loss (task #14, caught on the real
//! store: 7 tasks in `kanban.db`, 13 in the pair).
//!
//! The design law applied to the human: needing to know what a write-ahead log is in order
//! to back up your own history is exactly the internal knowledge a good tool does not
//! demand.
//!
//! Two formats, because they answer different questions:
//!
//! * **`backup`** -- a single consistent `.db`. Byte-for-byte fidelity, restores by being
//!   copied back, and needs no code in this file to stay in sync with the schema. This is
//!   the one to reach for.
//! * **`export`/`import`** -- JSON. Survives a schema the source store never had, can be
//!   read and diffed by a person, and can carry one project rather than all of them. It
//!   pays for that by being a translation, and translations lose things: see
//!   `IMPORT_SKIPS_CLAIMED_PATHS` below.

use crate::core::error::{Error, Result};
use crate::core::event::HOUSEKEEPING_KINDS;
use crate::core::model::*;
use crate::core::store::{now, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The envelope version, deliberately **not** the schema version.
///
/// They move for different reasons: the schema changes when the store gains a column, this
/// changes when the shape of the JSON changes. Tying them together would force an envelope
/// bump for every internal migration and leave an importer unable to say which of the two
/// it actually cannot read.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Export {
    pub format: u32,
    /// What the source store's schema was. Advisory: an importer reads by field name, so a
    /// newer export loses only the fields this binary does not know about.
    pub schema_version: i64,
    pub exported_at: i64,
    pub projects: Vec<ProjectExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectExport {
    /// `key`, not `id`. Ids are row numbers in one particular store and mean nothing in
    /// another; the key (git remote, else repo root) is the identity that travels.
    pub key: String,
    pub name: String,
    pub created_at: i64,
    /// Absolute paths on the machine that exported. Kept because they are what stops a
    /// worktree or second clone from forking the board, and a same-machine restore that
    /// dropped them would quietly re-introduce that split.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Exported in full rather than inferred from the tasks that reference them: a
    /// workstream with no open work still carries its name and the fact that it was
    /// closed, and an export that reconstructed the list from task rows would silently
    /// drop every empty one.
    ///
    /// `serde(default)` because exports written before migration 005 have no such field,
    /// and an importer that rejected them would turn a new column into a broken restore.
    #[serde(default)]
    pub workstreams: Vec<WorkstreamExport>,
    pub tasks: Vec<TaskExport>,
    pub notes: Vec<NoteExport>,
    pub events: Vec<EventExport>,
}

/// Carried by **name**, not id. Ids are row numbers in one particular store; the name is
/// what identifies a workstream inside its board, and it is already normalized and unique
/// there -- so it survives a round trip into a store that numbers its rows differently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkstreamExport {
    pub name: String,
    pub created_at: i64,
    pub closed_at: Option<i64>,
}

/// Rows carry their **source** id, and only so that references inside the same export can
/// be resolved. Import allocates its own ids and rewrites every reference through a map;
/// nothing here is inserted with the id it arrived with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskExport {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub status: Status,
    pub r#type: TaskType,
    pub origin: Origin,
    pub priority: Priority,
    pub blocked_by: Option<i64>,
    /// The workstream's **name**, for the same reason `key` is used for a project: an id
    /// means nothing outside the store that allocated it. `serde(default)` keeps pre-005
    /// exports importable.
    #[serde(default)]
    pub workstream: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteExport {
    pub id: i64,
    pub task_id: Option<i64>,
    pub title: String,
    pub body: String,
    pub tags: String,
    #[serde(default)]
    pub paths: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventExport {
    pub task_id: Option<i64>,
    pub note_id: Option<i64>,
    pub ts: i64,
    pub actor: Actor,
    pub kind: String,
    pub body: String,
}

/// What an import actually did. Returned rather than printed -- core computes, adapters
/// render.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportReport {
    pub projects: Vec<ImportedProject>,
    /// Projects present in the file that already existed here, so nothing was written for
    /// them. Named so the caller can say which, rather than "some were skipped".
    pub skipped_existing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedProject {
    pub key: String,
    pub name: String,
    pub tasks: usize,
    pub notes: usize,
    pub events: usize,
    /// Paths in the export that another project here already claims, and so were left
    /// alone. `project_paths.path` is a primary key -- one directory maps to exactly one
    /// board -- so taking a claimed path would redirect a *different* board's future
    /// sessions into this one. Skipping loses an alias, which the next session in that
    /// directory restores anyway.
    pub paths_skipped: Vec<String>,
}

impl Store {
    /// A consistent single-file copy, safe to take while sessions are writing.
    ///
    /// `VACUUM INTO` rather than a file copy: it goes through SQLite, so it folds in
    /// whatever is still sitting in the WAL and cannot catch a torn page. That is the
    /// entire point -- the failure this replaces was a copy that looked like it worked.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            // VACUUM INTO refuses an existing file, but it reports it as a bare SQLite
            // error. Say it in terms of the thing the caller passed.
            return Err(Error::Other(format!(
                "{} already exists -- backup will not overwrite it",
                dest.display()
            )));
        }
        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let path = dest.to_str().ok_or_else(|| {
            Error::Other("backup path is not valid UTF-8".into())
        })?;
        self.conn.execute("VACUUM INTO ?1", [path])?;
        Ok(())
    }

    /// Flush the WAL back into the main database file.
    ///
    /// Best-effort by design. It fails when another session holds the lock, and on a
    /// read-only connection it cannot run at all -- neither is worth surfacing, because the
    /// next clean exit does the same job. It exists to make the *common* case true: that
    /// `kanban.db` on its own is current, so the naive backup people will take anyway is
    /// usually not a data loss.
    ///
    /// It is not a substitute for `backup_to`, and nothing should treat it as one.
    /// PASSIVE, not TRUNCATE, and the difference only shows up under contention.
    ///
    /// Measured on the real store (2MB, and a 41MB synthetic copy) rather than reasoned
    /// about. Uncontended the two are indistinguishable -- ~0.5ms each, and the size of the
    /// database does not move the number, because a checkpoint's work is the WAL, not the
    /// main file. With one reader holding an open snapshot the picture inverts: TRUNCATE
    /// blocked for the full 5s `busy_timeout` and then reported busy, having folded in the
    /// same 182 pages that PASSIVE folded in, without blocking, in 1.5ms.
    ///
    /// So TRUNCATE bought nothing here and could stall a process for five seconds on its
    /// way out -- and this store is global, shared by every session on the machine, which
    /// is what makes a reader holding a snapshot ordinary rather than exotic. What this
    /// call is for (task #14: leave the main file current so the naive `cp kanban.db` is
    /// not a silent rollback) is achieved by folding the pages in. Resetting the WAL file
    /// afterwards was never the point, and `backup_to` remains the correct backup path.
    pub fn checkpoint(&self) {
        let _ = self.conn.pragma_update(None, "wal_checkpoint", "PASSIVE");
    }

    /// One project, or every project when `keys` is empty.
    pub fn export(&self, keys: &[String]) -> Result<Export> {
        let mut projects = Vec::new();
        for p in self.all_projects()? {
            if !keys.is_empty() && !keys.iter().any(|k| k == &p.key || k == &p.name) {
                continue;
            }
            projects.push(self.export_project(&p)?);
        }
        if projects.is_empty() && !keys.is_empty() {
            return Err(Error::ProjectNotFound {
                query: keys.join(", "),
                existing: self.all_projects()?.into_iter().map(|p| p.name).collect(),
            });
        }
        Ok(Export {
            format: FORMAT_VERSION,
            schema_version: self.schema_version()?,
            exported_at: now(),
            projects,
        })
    }

    fn export_project(&self, p: &Project) -> Result<ProjectExport> {
        let mut st = self.conn.prepare(
            "SELECT path FROM project_paths WHERE project_id = ?1 ORDER BY path",
        )?;
        let paths: Vec<String> =
            st.query_map([p.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;

        let mut st = self.conn.prepare(
            "SELECT name, created_at, closed_at FROM workstreams WHERE project_id = ?1 ORDER BY id",
        )?;
        let workstreams: Vec<WorkstreamExport> = st
            .query_map([p.id], |r| {
                Ok(WorkstreamExport { name: r.get(0)?, created_at: r.get(1)?, closed_at: r.get(2)? })
            })?
            .collect::<rusqlite::Result<_>>()?;

        // Ordered by id everywhere below: an export that reorders between two runs is one
        // nobody can diff, which is half the reason the JSON format exists at all.
        let mut st = self.conn.prepare(
            "SELECT t.id, t.title, t.body, t.status, t.type, t.origin, t.priority, t.blocked_by,
                    t.created_at, t.updated_at, w.name
               FROM tasks t LEFT JOIN workstreams w ON w.id = t.workstream_id
              WHERE t.project_id = ?1 ORDER BY t.id",
        )?;
        let tasks: Vec<TaskExport> = st
            .query_map([p.id], |r| {
                Ok(TaskExport {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    body: r.get(2)?,
                    status: r.get(3)?,
                    r#type: r.get(4)?,
                    origin: r.get(5)?,
                    priority: r.get(6)?,
                    blocked_by: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                    workstream: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut st = self.conn.prepare(
            "SELECT id, task_id, title, body, tags, created_at, updated_at
               FROM notes WHERE project_id = ?1 ORDER BY id",
        )?;
        let mut notes: Vec<NoteExport> = st
            .query_map([p.id], |r| {
                Ok(NoteExport {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    title: r.get(2)?,
                    body: r.get(3)?,
                    tags: r.get(4)?,
                    paths: Vec::new(),
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        for n in &mut notes {
            let mut st = self
                .conn
                .prepare("SELECT path FROM note_paths WHERE note_id = ?1 ORDER BY path")?;
            n.paths = st.query_map([n.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        }

        let mut st = self.conn.prepare(
            "SELECT task_id, note_id, ts, actor, kind, body
               FROM events WHERE project_id = ?1 ORDER BY id",
        )?;
        let events: Vec<EventExport> = st
            .query_map([p.id], |r| {
                Ok(EventExport {
                    task_id: r.get(0)?,
                    note_id: r.get(1)?,
                    ts: r.get(2)?,
                    actor: r.get(3)?,
                    kind: r.get(4)?,
                    body: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        Ok(ProjectExport {
            key: p.key.clone(),
            name: p.name.clone(),
            created_at: p.created_at,
            paths,
            workstreams,
            tasks,
            notes,
            events,
        })
    }

    /// Restore projects that are **not** already here.
    ///
    /// A project whose key already exists is skipped whole, never merged. Merging two
    /// histories of the same board is a genuinely different problem -- which task survives
    /// when both sides changed one, how two event logs interleave -- and answering it
    /// halfway inside an importer would leave two half-answers instead of one. That is
    /// task #3, and until it exists this refuses rather than guesses.
    pub fn import(&self, export: &Export) -> Result<ImportReport> {
        if export.format > FORMAT_VERSION {
            return Err(Error::Other(format!(
                "export format v{} is newer than this binary understands (v{FORMAT_VERSION})",
                export.format
            )));
        }
        let mut report = ImportReport::default();
        for pe in &export.projects {
            if self.project_id_by_key(&pe.key)?.is_some() {
                report.skipped_existing.push(pe.key.clone());
                continue;
            }
            report.projects.push(self.import_project(pe)?);
        }
        Ok(report)
    }

    fn project_id_by_key(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM projects WHERE key = ?1", [key], |r| r.get(0))
            .ok())
    }

    fn import_project(&self, pe: &ProjectExport) -> Result<ImportedProject> {
        self.conn.execute(
            "INSERT INTO projects (key, name, created_at) VALUES (?1, ?2, ?3)",
            params![pe.key, pe.name, pe.created_at],
        )?;
        let pid = self.conn.last_insert_rowid();

        let mut paths_skipped = Vec::new();
        for path in &pe.paths {
            let claimed: Option<i64> = self
                .conn
                .query_row("SELECT project_id FROM project_paths WHERE path = ?1", [path], |r| {
                    r.get(0)
                })
                .ok();
            match claimed {
                Some(_) => paths_skipped.push(path.clone()),
                None => {
                    self.conn.execute(
                        "INSERT INTO project_paths (path, project_id, created_at) VALUES (?1, ?2, ?3)",
                        params![path, pid, pe.created_at],
                    )?;
                }
            }
        }

        // Workstreams before tasks: a task carries its workstream by name, so the row it
        // points at has to exist before the task is inserted. Inserted directly rather than
        // through `ensure_workstream` because that writes a `workstream_created` event, and
        // a restore must not manufacture history that did not happen -- the export's own
        // events are replayed further down.
        let mut workstream_ids = std::collections::HashMap::new();
        for w in &pe.workstreams {
            self.conn.execute(
                "INSERT OR IGNORE INTO workstreams (project_id, name, created_at, closed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![pid, w.name, w.created_at, w.closed_at],
            )?;
            let id: i64 = self.conn.query_row(
                "SELECT id FROM workstreams WHERE project_id = ?1 AND name = ?2",
                params![pid, w.name], |r| r.get(0),
            )?;
            workstream_ids.insert(w.name.clone(), id);
        }

        // Tasks in two passes. `blocked_by` points at another task in the same export, so
        // it cannot be written until every id in the project has been allocated.
        let mut task_ids = std::collections::HashMap::new();
        for t in &pe.tasks {
            // A name with no workstream row is dropped rather than invented, matching how
            // a dangling `blocked_by` is handled: losing the grouping degrades the board,
            // while inventing a workstream would put the task in one that never existed.
            let ws = t.workstream.as_ref().and_then(|n| workstream_ids.get(n));
            self.conn.execute(
                "INSERT INTO tasks (project_id, title, body, status, type, origin, priority, workstream_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![pid, t.title, t.body, t.status, t.r#type, t.origin, t.priority, ws, t.created_at, t.updated_at],
            )?;
            task_ids.insert(t.id, self.conn.last_insert_rowid());
        }
        for t in &pe.tasks {
            // A dangling reference is dropped rather than carried. `blocked_by` is
            // annotation -- `status` is authoritative -- so losing one degrades the board;
            // pointing it at whatever row happens to hold that id corrupts it.
            let Some(new) = t.blocked_by.and_then(|b| task_ids.get(&b)) else { continue };
            self.conn.execute(
                "UPDATE tasks SET blocked_by = ?1 WHERE id = ?2",
                params![new, task_ids[&t.id]],
            )?;
        }

        let mut note_ids = std::collections::HashMap::new();
        for n in &pe.notes {
            self.conn.execute(
                "INSERT INTO notes (project_id, task_id, title, body, tags, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    pid,
                    n.task_id.and_then(|t| task_ids.get(&t)).copied(),
                    n.title, n.body, n.tags, n.created_at, n.updated_at
                ],
            )?;
            let nid = self.conn.last_insert_rowid();
            note_ids.insert(n.id, nid);
            for path in &n.paths {
                self.conn.execute(
                    "INSERT OR IGNORE INTO note_paths (note_id, path) VALUES (?1, ?2)",
                    params![nid, path],
                )?;
            }
        }

        for e in &pe.events {
            self.conn.execute(
                "INSERT INTO events (project_id, task_id, note_id, ts, actor, kind, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    pid,
                    e.task_id.and_then(|t| task_ids.get(&t)).copied(),
                    e.note_id.and_then(|n| note_ids.get(&n)).copied(),
                    e.ts, e.actor, e.kind, e.body
                ],
            )?;
        }

        // The import is itself a mutation, so it writes an event -- otherwise a live page
        // watching `MAX(events.id)` would never notice a whole board appearing.
        //
        // Deliberately NOT in HOUSEKEEPING_KINDS. "This board arrived from a backup on
        // 12 March" is the first thing that explains why history stops dead at a date, and
        // an agent reading `recent` cold is exactly who needs it.
        debug_assert!(!HOUSEKEEPING_KINDS.contains(&"imported"));
        self.write_event(
            pid,
            None,
            Actor::System,
            "imported",
            &format!(
                "Imported from an export: {} tasks, {} notes, {} events.",
                pe.tasks.len(),
                pe.notes.len(),
                pe.events.len()
            ),
        )?;

        Ok(ImportedProject {
            key: pe.key.clone(),
            name: pe.name.clone(),
            tasks: pe.tasks.len(),
            notes: pe.notes.len(),
            events: pe.events.len(),
            paths_skipped,
        })
    }
}
