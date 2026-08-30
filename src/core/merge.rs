//! Repairing a board that already split in two.
//!
//! `project_paths` exists to stop a split: a worktree, a second clone or a moved folder
//! learns its way onto the board it belongs to instead of minting a new one. That is
//! prevention, and prevention is one-way -- it does nothing for a store where the split
//! already happened, which is every store that predates the alias learning, and any store
//! where someone worked in a clone before the two were ever connected.
//!
//! The symptom is not an error. It is a board that is quietly half a memory: an agent
//! starting cold reads one of the two and concludes that is all there is. That is the exact
//! failure this project exists to prevent, which is why repairing it is worth a command.
//!
//! **Scope: two projects in one store.** Both sides are local rows, so the task id spaces
//! are already disjoint and nothing has to be renumbered -- a merge here is reparenting.
//! Merging histories that came from *different* stores is a genuinely different problem
//! (ids collide meaninglessly, and "both sides edited task #7" has no answer in the data);
//! it is filed separately rather than approximated here.

use crate::core::error::{Error, Result};
use crate::core::model::{Actor, Project};
use crate::core::store::Store;
use rusqlite::params;

/// What a merge did, and what it cost. Returned rather than printed: core computes,
/// adapters render.
#[derive(Debug, Clone)]
pub struct MergeReport {
    pub into: Project,
    pub merged: Project,
    pub tasks: usize,
    pub notes: usize,
    pub events: usize,
    pub paths: usize,
}

impl Store {
    /// Move everything from `from` onto `into`, then delete the emptied project.
    ///
    /// Which side survives is the caller's decision because it is visible in the result:
    /// the surviving `key` is what future directories resolve against. `merge_projects_auto`
    /// picks for the common case.
    pub fn merge_projects(&self, into: i64, from: i64) -> Result<MergeReport> {
        if into == from {
            return Err(Error::Other("a board cannot be merged into itself".into()));
        }
        let target = self.project(into)?;
        let source = self.project(from)?;

        let counted = |table: &str| -> Result<usize> {
            Ok(self.conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?1"),
                [from],
                |r| r.get::<_, i64>(0),
            )? as usize)
        };
        let tasks = counted("tasks")?;
        let notes = counted("notes")?;
        let events = counted("events")?;
        let paths = counted("project_paths")?;

        // Order is load-bearing. Every child table cascades on DELETE of a project, and
        // `tasks.blocked_by` is ON DELETE SET NULL -- so deleting the source first would
        // silently drop exactly the cross-board references a split produces (someone filed
        // the blocker on one half and the blocked task on the other). Reparent everything,
        // and only then remove a project that owns nothing.
        let tx = self.conn.unchecked_transaction()?;

        // Reparenting fires the FTS `_au` triggers, which re-index title and body with
        // unchanged rowids. Wasted work, not wrong work -- and cheaper than teaching the
        // triggers about a column they do not index.
        for table in ["tasks", "notes", "events"] {
            tx.execute(
                &format!("UPDATE {table} SET project_id = ?1 WHERE project_id = ?2"),
                params![into, from],
            )?;
        }
        // An UPDATE, not an insert: `project_paths.path` is the primary key, so every path
        // already belongs to exactly one project and a collision is impossible here.
        tx.execute(
            "UPDATE project_paths SET project_id = ?1 WHERE project_id = ?2",
            params![into, from],
        )?;
        tx.execute("DELETE FROM projects WHERE id = ?1", [from])?;
        tx.commit()?;

        // Deliberately not housekeeping. This is the entry that explains why a board's task
        // ids have gaps, why two narratives interleave mid-history, and where a key that no
        // longer resolves went. An agent reading `recent` cold is precisely who needs it.
        self.write_event(
            into,
            None,
            Actor::System,
            "project_merged",
            &format!(
                "Merged board \"{}\" ({}) into this one: {tasks} tasks, {notes} notes, \
                 {events} events, {paths} paths. That board no longer exists.",
                source.name, source.key
            ),
        )?;

        Ok(MergeReport { into: target, merged: source, tasks, notes, events, paths })
    }

    /// `merge_projects` with the survivor chosen: the older board wins.
    ///
    /// The original is the one with history worth keeping and the key other things already
    /// resolve against; the split is the accident. Ties break on the lower id, which is the
    /// same rule stated for rows created in the same second.
    pub fn merge_projects_auto(&self, a: i64, b: i64) -> Result<MergeReport> {
        let (pa, pb) = (self.project(a)?, self.project(b)?);
        let a_first = (pa.created_at, pa.id) <= (pb.created_at, pb.id);
        if a_first { self.merge_projects(a, b) } else { self.merge_projects(b, a) }
    }
}
