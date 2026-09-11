//! Removing things, permanently.
//!
//! # Why this exists on a project whose first priority is history
//!
//! It should not, by the stated priorities — and for a long time it did not. `archived`
//! covers "done with this", and a note has no lifecycle at all because knowledge is supposed
//! to outlive the work that produced it.
//!
//! What forced it (task #15): **nothing could ever be removed from this store.**
//! `update_note` records the previous body in a `note_updated` event so a superseded fact
//! stays recoverable — correct for knowledge, and it means overwriting a note *preserves*
//! what you were trying to replace. Event bodies are FTS-indexed, so the old content stays
//! searchable through `recall` forever. Agents record what they read, the store is global
//! across every project on the machine, and there was no operation anywhere that made
//! anything actually go away.
//!
//! So this is not a delete button for tidiness. It is the escape hatch, and it is the one
//! operation in the system that destroys history on purpose.
//!
//! # The rules it follows
//!
//! * **Nothing is exposed over MCP.** Destroying history is a deliberate human act, and an
//!   agent able to delete history is an agent able to cover its own tracks. The HTTP API is
//!   the only caller.
//! * **Deletion is explicit, never left to `ON DELETE CASCADE`.** The foreign key cascades
//!   would mostly do the right thing, but `PRAGMA foreign_keys` is per-connection state, and
//!   a forget that silently leaves content behind because a pragma did not stick is exactly
//!   the failure this exists to prevent. The cascades stay as a backstop.
//! * **The tombstone carries no content.** Just `note #12`. A tombstone quoting the thing it
//!   is recording the removal of would defeat the entire operation.
//! * **Forgetting a task is not forgetting what was learned doing it.** Notes attached to it
//!   survive, detached, and so do their own histories.

use crate::core::error::Result;
use crate::core::model::Actor;
use crate::core::store::Store;
use rusqlite::{params, OptionalExtension};

impl Store {
    /// Permanently removes a note and every event carrying its content.
    ///
    /// Not recoverable. The only copy afterwards is whatever backup existed before the call.
    pub fn forget_note(&self, project_id: i64, id: i64) -> Result<()> {
        // Proves the note exists *and* is on this board. The store is global, so an id alone
        // would let a caller destroy another project's note while naming this one.
        let note = self.note(project_id, id)?;

        let tx = self.conn.unchecked_transaction()?;
        // The `note_updated` bodies live here, each embedding a previous version of the note.
        // This is the delete that actually matters; removing the note row alone would leave
        // every superseded body behind, indexed and searchable.
        tx.execute("DELETE FROM events WHERE note_id = ?1", [id])?;
        tx.execute("DELETE FROM note_paths WHERE note_id = ?1", [id])?;
        tx.execute("DELETE FROM notes WHERE id = ?1 AND project_id = ?2", params![id, project_id])?;
        tx.commit()?;

        // Written after the delete so the cascade cannot take it. `task_id` is kept so the
        // removal shows up in the history of the task the note belonged to; there is no
        // `note_id`, because the note it would point at no longer exists.
        self.write_event(project_id, note.task_id, Actor::User, "forgotten", &format!("note #{id}"))?;
        Ok(())
    }

    /// Permanently removes a task and its history. Notes made while working on it survive.
    pub fn forget_task(&self, project_id: i64, id: i64) -> Result<()> {
        self.task(project_id, id)?;

        let tx = self.conn.unchecked_transaction()?;

        // Detach note history *first*. These events sit under this task's `task_id`, so the
        // delete below — or the FK cascade — would otherwise take a note's own history along
        // with the task, leaving a surviving note whose past silently vanished.
        tx.execute(
            "UPDATE events SET task_id = NULL WHERE task_id = ?1 AND note_id IS NOT NULL",
            [id],
        )?;
        tx.execute("DELETE FROM events WHERE task_id = ?1", [id])?;
        // Knowledge outlives the task that produced it — the reason notes are a separate
        // entity in the first place.
        tx.execute("UPDATE notes SET task_id = NULL WHERE task_id = ?1", [id])?;
        // `blocked_by` is an annotation, so a dangling reference would render as a blocker
        // that cannot be looked up rather than failing loudly.
        tx.execute("UPDATE tasks SET blocked_by = NULL WHERE blocked_by = ?1", [id])?;
        // Explicit for the reason stated in the module header. Left behind, a link would keep
        // counting in the repos menu for a ticket that no longer exists.
        tx.execute("DELETE FROM task_repos WHERE task_id = ?1", [id])?;
        tx.execute("DELETE FROM tasks WHERE id = ?1 AND project_id = ?2", params![id, project_id])?;
        tx.commit()?;

        self.write_event(project_id, None, Actor::User, "forgotten", &format!("task #{id}"))?;
        Ok(())
    }

    /// Permanently removes a repo from every board it is on, and from every ticket naming it.
    ///
    /// Unlike `remove_repo`, which takes it off one board, this is the repo gone from the
    /// store. Its folder's path aliases stay: they belong to the home board, whose history
    /// still lives there, and dropping them would send the next session in that folder to a
    /// brand-new empty board.
    pub fn forget_repo(&self, id: i64) -> Result<()> {
        let r = self.repo_anywhere(id)?;
        let boards: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT project_id FROM board_repos WHERE repo_id = ?1 ORDER BY project_id")?;
            let rows = st.query_map([id], |row| row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM task_repos WHERE repo_id = ?1", [id])?;
        tx.execute("DELETE FROM board_repos WHERE repo_id = ?1", [id])?;
        tx.execute("DELETE FROM repos WHERE id = ?1", [r.id])?;
        // One tombstone per board that had it, so each board's history says why a repo left
        // its menu and its tickets.
        for pid in boards {
            self.write_event(pid, None, Actor::User, "forgotten", &format!("repo #{id}"))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Permanently removes a board: its tasks, notes, workstreams, history and path aliases.
    ///
    /// Repos it was home to and that other boards share pass to the earliest of them, so their
    /// folders keep opening on a board that has them; repos only it had go. Its own folders
    /// are released, so the next session opened in one starts a fresh board -- the point of
    /// forgetting a whole project is that nothing of it answers any more.
    ///
    /// There is no board left to hold a tombstone, so the change cursor is advanced directly
    /// instead (see `change_cursor`). Without that the delete would be invisible to every
    /// live page.
    pub fn forget_board(&self, project_id: i64) -> Result<()> {
        self.project(project_id)?;
        let homed: Vec<i64> = {
            let mut st = self.conn.prepare("SELECT id FROM repos WHERE home_project_id = ?1")?;
            let rows = st.query_map([project_id], |row| row.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let tx = self.conn.unchecked_transaction()?;
        for id in homed {
            let r = self.repo_anywhere(id)?;
            let next: Option<i64> = tx.query_row(
                "SELECT project_id FROM board_repos WHERE repo_id = ?1 AND project_id != ?2
                  ORDER BY created_at, project_id LIMIT 1",
                params![id, project_id],
                |row| row.get(0),
            ).optional()?;
            match next {
                Some(next) => {
                    self.rehome(&r, next)?;
                    // Names the repo, never the board: the board is what is being forgotten.
                    self.write_event(next, None, Actor::User, "repo_home",
                        &format!("{} now opens on this board; the board it opened on was forgotten", r.name))?;
                }
                None => {
                    tx.execute("DELETE FROM task_repos WHERE repo_id = ?1", [id])?;
                    tx.execute("DELETE FROM board_repos WHERE repo_id = ?1", [id])?;
                    tx.execute("DELETE FROM repos WHERE id = ?1", [id])?;
                }
            }
        }

        // Rows on OTHER boards pointing into this one. Every link is meant to stay on one
        // board, but a dangling one renders as a reference that cannot be looked up, so they
        // are cut rather than trusted to be absent.
        const TASKS: &str = "(SELECT id FROM tasks WHERE project_id = ?1)";
        const NOTES: &str = "(SELECT id FROM notes WHERE project_id = ?1)";
        tx.execute(&format!("UPDATE events SET task_id = NULL WHERE project_id != ?1 AND task_id IN {TASKS}"), [project_id])?;
        tx.execute(&format!("UPDATE events SET note_id = NULL WHERE project_id != ?1 AND note_id IN {NOTES}"), [project_id])?;
        tx.execute(&format!("UPDATE notes SET task_id = NULL WHERE project_id != ?1 AND task_id IN {TASKS}"), [project_id])?;
        tx.execute(&format!("UPDATE tasks SET blocked_by = NULL WHERE project_id != ?1 AND blocked_by IN {TASKS}"), [project_id])?;

        // Explicit, child tables first, for the reason in the module header.
        tx.execute("DELETE FROM events WHERE project_id = ?1", [project_id])?;
        tx.execute(&format!("DELETE FROM note_paths WHERE note_id IN {NOTES}"), [project_id])?;
        tx.execute("DELETE FROM notes WHERE project_id = ?1", [project_id])?;
        tx.execute(&format!("DELETE FROM task_repos WHERE task_id IN {TASKS}"), [project_id])?;
        tx.execute("DELETE FROM board_repos WHERE project_id = ?1", [project_id])?;
        tx.execute("UPDATE tasks SET blocked_by = NULL WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM current_workstream WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM board_denied_profiles WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM tasks WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM workstreams WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM project_paths WHERE project_id = ?1", [project_id])?;
        tx.execute("DELETE FROM projects WHERE id = ?1", [project_id])?;
        // The tombstone a board cannot have. `events` is AUTOINCREMENT, so its sequence is a
        // counter nothing else moves backwards; bumping it is a change every poller sees.
        tx.execute("UPDATE sqlite_sequence SET seq = seq + 1 WHERE name = 'events'", [])?;
        tx.commit()?;
        Ok(())
    }
}
