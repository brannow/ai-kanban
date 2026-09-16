//! The person's to-do list.
//!
//! This module is the exception to nearly every convention in `core`, and each exception is
//! deliberate. It writes no events, it takes no `project_id`, and no agent-facing surface
//! calls it: not `board`, not `render`, not the MCP server, not the hook. The whole point is
//! a list beside the work that the agent neither manages nor reports on.
//!
//! **The line this draws, stated honestly.** The store is one SQLite file, and an agent with
//! a shell can read any file. What is enforceable is the *surface*: there is no tool, no
//! prompt and no rendered line through which an agent sees or changes a to-do, so nothing it
//! does on its own initiative can touch this list. That is the guarantee -- not encryption.
//!
//! Checked items are swept a day after they are checked, so the list clears itself without a
//! chore. See `SWEEP_AFTER`.

use crate::core::error::{Error, Result};
use crate::core::model::Todo;
use crate::core::store::{now, Store};
use rusqlite::{params, OptionalExtension};

/// How long a checked to-do stays on the list before it is swept.
///
/// A day, because the value of leaving it there is seeing what got done today and being able
/// to undo a mis-click -- both of which expire on roughly that scale.
pub const SWEEP_AFTER: i64 = 24 * 60 * 60;

const TODO_COLS: &str = "id, text, done_at, created_at, updated_at";

fn row_to_todo(r: &rusqlite::Row<'_>) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: r.get(0)?,
        text: r.get(1)?,
        done_at: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

impl Store {
    /// The list: unchecked first in the order they were added, checked ones beneath.
    ///
    /// Sweeps on the way, which is the only thing that ever removes an expired item. A
    /// background timer would be a second moving part for a list nobody is looking at while
    /// the page is closed -- and the sweep is only observable when somebody reads the list.
    pub fn todos(&self) -> Result<Vec<Todo>> {
        self.sweep_todos()?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {TODO_COLS} FROM todos
              ORDER BY done_at IS NOT NULL, done_at, created_at, id"
        ))?;
        Ok(st.query_map([], row_to_todo)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn todo(&self, id: i64) -> Result<Todo> {
        self.todo_opt(id)?.ok_or(Error::TodoNotFound { id })
    }

    pub fn todo_opt(&self, id: i64) -> Result<Option<Todo>> {
        Ok(self.conn
            .query_row(&format!("SELECT {TODO_COLS} FROM todos WHERE id = ?1"), [id], row_to_todo)
            .optional()?)
    }

    /// Adds an item. Blank text is refused rather than stored: an empty row on a checklist is
    /// unclickable and unexplainable, and the caller always has a person to tell.
    pub fn add_todo(&self, text: &str) -> Result<Todo> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::InvalidValue {
                field: "text",
                value: text.to_string(),
                valid: "anything but blank".into(),
            });
        }
        let ts = now();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO todos (text, done_at, created_at, updated_at) VALUES (?1, NULL, ?2, ?2)",
            params![text, ts],
        )?;
        let id = tx.last_insert_rowid();
        bump(&tx)?;
        tx.commit()?;
        self.todo(id)
    }

    /// Checks or unchecks. Idempotent: setting the state it already has is not an error, it
    /// just does not move `done_at` -- so a double-click cannot extend an item's stay.
    pub fn set_todo_done(&self, id: i64, done: bool) -> Result<Todo> {
        let before = self.todo(id)?;
        if before.done_at.is_some() == done {
            return Ok(before);
        }
        let ts = now();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE todos SET done_at = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, if done { Some(ts) } else { None }, ts],
        )?;
        bump(&tx)?;
        tx.commit()?;
        self.todo(id)
    }

    /// Edits the text. Unchecked or not -- fixing a typo on something you already did is
    /// still fixing a typo.
    pub fn edit_todo(&self, id: i64, text: &str) -> Result<Todo> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::InvalidValue {
                field: "text",
                value: text.to_string(),
                valid: "anything but blank".into(),
            });
        }
        self.todo(id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE todos SET text = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, text, now()],
        )?;
        bump(&tx)?;
        tx.commit()?;
        self.todo(id)
    }

    /// Gone for good. No tombstone and no event: this list has no history, by design -- a
    /// person removing an errand is not recording anything, and `docs/vision.md` is explicit
    /// that the memory this project keeps is memory about the *work*.
    pub fn remove_todo(&self, id: i64) -> Result<()> {
        self.todo(id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM todos WHERE id = ?1", [id])?;
        bump(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Drops checked items older than `SWEEP_AFTER`. Returns how many went.
    ///
    /// Bumps the revision only when it actually deleted something, so an idle store does not
    /// wake every open page every time somebody looks at the list.
    pub fn sweep_todos(&self) -> Result<usize> {
        self.sweep_todos_before(now() - SWEEP_AFTER)
    }

    /// The cutoff as a parameter, so a test can put the boundary where it likes instead of
    /// waiting a day or writing a fake clock into the store.
    pub fn sweep_todos_before(&self, cutoff: i64) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let gone = tx.execute("DELETE FROM todos WHERE done_at IS NOT NULL AND done_at < ?1", [cutoff])?;
        if gone > 0 {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok(gone)
    }

    /// The list's change counter, which the live stream polls the way it polls
    /// `MAX(events.id)` for everything else. See `migrations/011_todos.sql`.
    pub fn todo_rev(&self) -> Result<i64> {
        Ok(self.conn
            .query_row("SELECT rev FROM todo_rev WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }
}

/// Every write goes through this. Inside the same transaction as the write it describes, so a
/// rolled-back change cannot leave a revision claiming something happened.
fn bump(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute("UPDATE todo_rev SET rev = rev + 1 WHERE id = 1", [])?;
    Ok(())
}
