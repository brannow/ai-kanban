//! Task lifecycle, and the events it generates.
//!
//! Every mutation writes an event. That is not bookkeeping for its own sake: the project's
//! first priority is history, so "what happened and why" has to be a byproduct of doing the
//! work rather than a second call the agent must remember to make.

use crate::core::error::{Error, Result};
use crate::core::model::*;
use crate::core::store::{now, Store};
use rusqlite::{params, OptionalExtension};

/// A new task. Everything except the title has a defensible default, because every
/// argument the agent has to think about is friction on the path that matters most
/// (spotting a side quest and filing it without breaking focus).
#[derive(Debug, Clone)]
pub struct TaskDraft {
    pub title: String,
    pub body: String,
    pub task_type: TaskType,
    pub origin: Origin,
    pub priority: Priority,
    pub status: Status,
    pub blocked_by: Option<i64>,
}

impl TaskDraft {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: String::new(),
            task_type: TaskType::default(),
            origin: Origin::default(),
            priority: Priority::default(),
            status: Status::default(),
            blocked_by: None,
        }
    }
}

/// A change. `None` means "leave alone"; the nested `Option` on `blocked_by` distinguishes
/// "leave alone" from "clear it".
#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub task_type: Option<TaskType>,
    pub blocked_by: Option<Option<i64>>,
    /// The *why*. Recorded as the event body -- this is the field that makes the history
    /// worth reading six months later.
    pub log: Option<String>,
    pub actor: Actor,
}

impl TaskPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.status.is_none()
            && self.priority.is_none() && self.task_type.is_none() && self.blocked_by.is_none()
            && self.log.is_none()
    }
}

const TASK_COLS: &str = "id, project_id, title, body, status, type, origin, priority, blocked_by, created_at, updated_at";

pub(crate) fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        body: r.get(3)?,
        status: r.get(4)?,
        task_type: r.get(5)?,
        origin: r.get(6)?,
        priority: r.get(7)?,
        blocked_by: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

impl Store {
    pub fn create_task(&self, project_id: i64, draft: TaskDraft) -> Result<Task> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO tasks (project_id, title, body, status, type, origin, priority, blocked_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                project_id, draft.title, draft.body, draft.status, draft.task_type,
                draft.origin, draft.priority, draft.blocked_by, ts
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        let actor = match draft.origin { Origin::User => Actor::User, Origin::Agent => Actor::Agent };
        self.write_event(project_id, Some(id), actor, "created", &draft.title)?;
        self.task(project_id, id)
    }

    /// Fetches a task, **scoped to a project**.
    ///
    /// The project is a parameter rather than something looked up from the id because the
    /// store is global: several projects share one database, and a bare id lookup would
    /// happily return -- or splice into a board -- a task belonging to a different one.
    /// Requiring the caller to say which board it means makes that impossible to get
    /// wrong, and it is free: every adapter resolves a project before it does anything.
    pub fn task(&self, project_id: i64, id: i64) -> Result<Task> {
        self.task_opt(project_id, id)?.ok_or_else(|| self.task_not_found(project_id, id))
    }

    pub fn task_opt(&self, project_id: i64, id: i64) -> Result<Option<Task>> {
        Ok(self.conn
            .query_row(
                &format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?1 AND project_id = ?2"),
                params![id, project_id],
                row_to_task,
            )
            .optional()?)
    }

    /// Builds the not-found error with the tasks that *do* exist **on this board**, so the
    /// adapter can correct the mistake instead of merely reporting it. Listing tasks from
    /// other projects here would be worse than listing none: it would suggest ids that
    /// still do not work, and give no clue why.
    fn task_not_found(&self, project_id: i64, id: i64) -> Error {
        let existing = self.conn
            .prepare("SELECT id, title FROM tasks WHERE project_id = ?1 AND status != 'archived' ORDER BY updated_at DESC LIMIT 8")
            .and_then(|mut st| {
                st.query_map([project_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        let project = self.project(project_id).map(|p| p.name).unwrap_or_default();
        Error::TaskNotFound { id, project, existing }
    }

    /// Applies a change and records why. Returns the updated task.
    ///
    /// One call covers status, priority, body *and* the log entry, because splitting them
    /// would make recording the reason a separate call -- and a separate call is the one
    /// an agent under time pressure skips.
    pub fn update_task(&self, project_id: i64, id: i64, patch: TaskPatch) -> Result<Task> {
        let before = self.task(project_id, id)?;
        if patch.is_empty() {
            return Ok(before);
        }

        if let Some(Some(blocker)) = patch.blocked_by {
            if blocker == id {
                return Err(Error::Other("a task cannot block itself".into()));
            }
            // Same-project check, not merely existence: a cross-project blocker would
            // render as "#12" on a board where #12 is a different task entirely.
            if self.task_opt(project_id, blocker)?.is_none() {
                return Err(self.task_not_found(project_id, blocker));
            }
        }

        let ts = now();
        let title = patch.title.clone().unwrap_or_else(|| before.title.clone());
        let body = patch.body.clone().unwrap_or_else(|| before.body.clone());
        let status = patch.status.unwrap_or(before.status);
        let priority = patch.priority.unwrap_or(before.priority);
        let task_type = patch.task_type.unwrap_or(before.task_type);
        let blocked_by = match patch.blocked_by { Some(v) => v, None => before.blocked_by };

        self.conn.execute(
            "UPDATE tasks SET title=?2, body=?3, status=?4, type=?5, priority=?6, blocked_by=?7, updated_at=?8 WHERE id=?1",
            params![id, title, body, status, task_type, priority, blocked_by, ts],
        )?;

        // One event per call, not one per field. Per-field events would bury the reason in
        // noise; the reason is the part worth keeping.
        let kind = if patch.status.is_some() && status != before.status {
            // The new status rides in the kind so the board's `recent` section can render
            // `#12 -> doing` without re-deriving it from the task's *current* status,
            // which would be wrong for anything but the latest event.
            format!("status:{status}")
        } else {
            "updated".to_string()
        };
        let summary = patch.log.clone().unwrap_or_else(|| describe_change(&before, &title, &body, status, priority, blocked_by));
        self.write_event(before.project_id, Some(id), patch.actor, &kind, &summary)?;

        self.task(project_id, id)
    }

    pub fn tasks_blocked_by(&self, project_id: i64, id: i64) -> Result<Vec<Task>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE blocked_by = ?1 AND project_id = ?2 AND status != 'archived' ORDER BY updated_at DESC"
        ))?;
        Ok(st.query_map(params![id, project_id], row_to_task)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One task with everything needed to resume it cold.
    pub fn task_detail(&self, project_id: i64, id: i64) -> Result<TaskDetail> {
        let task = self.task(project_id, id)?;
        let project = self.project(task.project_id)?;
        let blocker = match task.blocked_by { Some(b) => self.task_opt(project_id, b)?, None => None };
        Ok(TaskDetail {
            blocking: self.tasks_blocked_by(project_id, id)?,
            events: self.task_events(id)?,
            notes: self.notes_for_task(project_id, id)?,
            project,
            task,
            blocker,
            now: now(),
        })
    }
}

/// Fallback event text when the caller gave no reason. States what changed, so the history
/// is at least factual -- but it is deliberately duller than a real `log`, because "what"
/// without "why" is the weaker half.
fn describe_change(before: &Task, title: &str, body: &str, status: Status, priority: Priority, blocked_by: Option<i64>) -> String {
    let mut parts = Vec::new();
    if status != before.status { parts.push(format!("{} -> {}", before.status, status)); }
    if priority != before.priority { parts.push(format!("priority {} -> {}", before.priority, priority)); }
    if blocked_by != before.blocked_by {
        parts.push(match blocked_by {
            Some(b) => format!("blocked by #{b}"),
            None => "unblocked".to_string(),
        });
    }
    if title != before.title { parts.push("title edited".to_string()); }
    if body != before.body { parts.push("body edited".to_string()); }
    if parts.is_empty() { "no change".to_string() } else { parts.join(", ") }
}
