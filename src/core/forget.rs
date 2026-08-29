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
use rusqlite::params;

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
        tx.execute("DELETE FROM tasks WHERE id = ?1 AND project_id = ?2", params![id, project_id])?;
        tx.commit()?;

        self.write_event(project_id, None, Actor::User, "forgotten", &format!("task #{id}"))?;
        Ok(())
    }
}
