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
    /// Freeform labels. No semantics an agent must honour -- that is what makes them safe
    /// to add without touching the fixed statuses. Stored comma-joined.
    pub tags: Vec<String>,
    /// Repo ids on this board. Adapters resolve names to ids with `resolve_repos`; core
    /// checks each belongs to the board, so a raw id from elsewhere cannot slip through.
    pub repos: Vec<i64>,
    /// The Planio ticket this task tracks.
    pub planio: Option<i64>,
}

impl TaskDraft {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: String::new(),
            tags: vec![],
            repos: vec![],
            planio: None,
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
    /// Move the task between workstreams. Nested `Option` like `blocked_by`: `None` leaves
    /// it alone, `Some(None)` makes it general project work, `Some(Some(id))` moves it.
    ///
    /// This exists because the agent inherits its workstream silently, which makes
    /// mis-filing the EXPECTED error rather than an edge case -- and without this it was
    /// the only field on a task that could never be corrected afterwards.
    pub workstream: Option<Option<i64>>,
    /// Replaces the whole set, like `NotePatch::tags`. Not add/remove: two verbs on one
    /// field is a surface an agent has to learn, and the set is short enough to resend.
    pub tags: Option<Vec<String>>,
    /// Replaces the whole set of repo ids, like `tags`. `Some(vec![])` clears them.
    pub repos: Option<Vec<i64>>,
    /// Nested like `blocked_by`: `Some(None)` clears the Planio ref.
    pub planio: Option<Option<i64>>,
    /// The *why*. Recorded as the event body -- this is the field that makes the history
    /// worth reading six months later.
    pub log: Option<String>,
    pub actor: Actor,
    /// The `version` the caller read, if it read one. `Some` makes the write a
    /// compare-and-swap that fails with `Error::Conflict` rather than overwriting a change
    /// it never saw.
    ///
    /// It is **optional on purpose**, and the two consumers answer it differently.
    ///
    /// The HTTP API always sends it: a browser form sits open for minutes while an agent
    /// works the same board, so a blind write there is a lost update waiting to happen
    /// (`docs/http-api.md`).
    ///
    /// The MCP agent sends `None`. Requiring a version would mean every `task_update` had
    /// to be preceded by a `task_show` to fetch one, turning one call into two -- and "one
    /// call per intent" is the rule the entire tool surface is built on. The exposure it
    /// accepts is small and bounded: an agent's read and write sit inside a single tool
    /// call milliseconds apart, and the MCP server serialises its own writes behind a mutex.
    pub expected_version: Option<i64>,
}

impl TaskPatch {
    /// Every field must be listed here. A field missing from this check makes a patch that
    /// changes only that field look like no change at all, and `update_task` returns early
    /// without writing -- silently, since nothing errors. `workstream` was added and missed
    /// exactly that way.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.status.is_none()
            && self.priority.is_none() && self.task_type.is_none() && self.blocked_by.is_none()
            && self.workstream.is_none()
            && self.tags.is_none()
            && self.repos.is_none()
            && self.planio.is_none()
            && self.log.is_none()
    }
}

pub(crate) const TASK_COLS: &str = "id, project_id, title, body, status, type, origin, priority, blocked_by, created_at, updated_at, version";

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
        version: r.get(11)?,
    })
}

impl Store {
    /// Files a task. The board's current workstream is **inherited**, never passed in.
    ///
    /// That is the whole reason `task_add` gained no new argument. Per `docs/tool-design.md`
    /// -- "every argument we don't bother the agent with is a win" -- and per note #12,
    /// filing is already the call least able to afford friction: it is the one an agent
    /// under pressure skips, and skipping it is the failure this project exists to fix.
    /// A workstream the agent has to remember to supply is one that ends up unset.
    pub fn create_task(&self, project_id: i64, draft: TaskDraft) -> Result<Task> {
        let workstream = self.current_workstream(project_id)?.map(|w| w.id);
        self.create_task_in(project_id, draft, workstream)
    }

    /// `create_task` with the workstream chosen explicitly rather than inherited.
    ///
    /// Only the human API uses this. An agent gets inheritance because an extra argument on
    /// the call it is most likely to skip is a cost the design will not pay; a person
    /// filling in a form can see the field and choose, including choosing "none".
    pub fn create_task_in(&self, project_id: i64, draft: TaskDraft, workstream: Option<i64>) -> Result<Task> {
        self.check_repos(project_id, &draft.repos)?;
        check_planio(draft.planio)?;
        let ts = now();
        self.conn.execute(
            "INSERT INTO tasks (project_id, title, body, status, type, origin, priority, blocked_by, workstream_id, tags, planio, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?11, ?12, ?10, ?10)",
            params![
                project_id, draft.title, draft.body, draft.status, draft.task_type,
                draft.origin, draft.priority, draft.blocked_by, workstream, ts,
                crate::core::note::join_tags(&draft.tags), draft.planio
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        if !draft.repos.is_empty() {
            self.set_task_repos(id, &draft.repos)?;
        }
        let actor = match draft.origin { Origin::User => Actor::User, Origin::Agent => Actor::Agent };
        self.write_event(project_id, Some(id), actor, "created", &draft.title)?;
        self.task(project_id, id)
    }

    /// A task's tags.
    ///
    /// A separate lookup because `tags` is deliberately not in `TASK_COLS` -- see migration
    /// 006. Only the paths that actually display tags pay for it, and the read-only
    /// SessionStart hook never names the column, which is what keeps MIN_READABLE_VERSION
    /// where it is.
    pub fn task_tags(&self, project_id: i64, id: i64) -> Result<Vec<String>> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT tags FROM tasks WHERE id = ?1 AND project_id = ?2",
            params![id, project_id],
            |r| r.get(0),
        ).optional()?;
        Ok(crate::core::note::split_tags(&raw.unwrap_or_default()))
    }

    /// Tags for a set of tasks, in one query.
    ///
    /// The batch form exists because the web board shows tags on every card while `tags` is
    /// out of `TASK_COLS`, so the rows it lists do not carry them. One lookup for the page
    /// beats one per card, and it stays one as the page size grows.
    pub fn tags_for(&self, project_id: i64, tasks: &[Task]) -> Result<Vec<(i64, Vec<String>)>> {
        if tasks.is_empty() {
            return Ok(vec![]);
        }
        let holes = tasks.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let mut st = self.conn.prepare(&format!(
            "SELECT id, tags FROM tasks WHERE project_id = ? AND id IN ({holes}) AND tags != ''"
        ))?;
        let ids = std::iter::once(project_id).chain(tasks.iter().map(|t| t.id)).collect::<Vec<_>>();
        let rows = st.query_map(rusqlite::params_from_iter(ids), |r| {
            Ok((r.get::<_, i64>(0)?, crate::core::note::split_tags(&r.get::<_, String>(1)?)))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
            .prepare("SELECT id, title FROM tasks WHERE project_id = ?1 AND status != 'archived' ORDER BY updated_at DESC, id DESC LIMIT 8")
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
        // Read separately because `workstream_id` is deliberately not in `TASK_COLS`, so
        // `before` does not carry it. See migration 005 on why that column stays out.
        let before_ws = self.task_workstream(project_id, id)?;
        let workstream = match patch.workstream { Some(v) => v, None => before_ws };
        // Read separately for the same reason as the workstream: `tags` is deliberately
        // out of `TASK_COLS`, so `before` does not carry it either.
        let tags = match &patch.tags {
            Some(t) => crate::core::note::join_tags(t),
            None => crate::core::note::join_tags(&self.task_tags(project_id, id)?),
        };
        if let Some(w) = workstream {
            // Same-board check, for the same reason `blocked_by` has one: the store is
            // global, and a workstream id from another board would file this task into a
            // group that does not exist on the board showing it.
            self.workstream(project_id, w)?;
        }
        if let Some(ids) = &patch.repos {
            self.check_repos(project_id, ids)?;
        }
        // Read separately for the same reason as tags: `planio` and the repo links are out of
        // `TASK_COLS` (migration 007), so `before` carries neither.
        let before_planio = self.task_planio(project_id, id)?;
        let planio = match patch.planio { Some(v) => v, None => before_planio };
        check_planio(planio)?;
        let before_repos: Vec<i64> = self.task_repos(project_id, id)?.into_iter().map(|r| r.id).collect();

        // `?9 IS NULL OR version = ?9` keeps the guarded and unguarded writes on one
        // statement: a caller that read a version gets a compare-and-swap, one that did not
        // gets the plain update. Two statements would be two chances to fix a bug once.
        let changed = self.conn.execute(
            "UPDATE tasks SET title=?2, body=?3, status=?4, type=?5, priority=?6, blocked_by=?7, updated_at=?8,
                    workstream_id=?10, tags=?11, planio=?12, version = version + 1
              WHERE id=?1 AND (?9 IS NULL OR version = ?9)",
            params![id, title, body, status, task_type, priority, blocked_by, ts, patch.expected_version, workstream, tags, planio],
        )?;
        if changed == 0 {
            // The row exists -- `self.task()` above proved it is here and on this board --
            // so the only way to match nothing is the version guard.
            let actual = self.task(project_id, id)?.version;
            return Err(Error::Conflict { id, expected: patch.expected_version.unwrap_or(0), actual });
        }
        if let Some(ids) = &patch.repos {
            self.set_task_repos(id, ids)?;
        }

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
        // Resolved to a NAME here rather than in `describe_change`, which has no store to
        // look one up with. An id in the history would be unreadable six months later,
        // which is the one thing this line exists to avoid.
        let mut lead = Vec::new();
        if workstream != before_ws {
            lead.push(match workstream {
                Some(w) => format!("moved to workstream \"{}\"", self.workstream(project_id, w)?.name),
                None => "moved out of its workstream".to_string(),
            });
        }
        if let Some(ids) = &patch.repos {
            let (mut after, mut was) = (ids.clone(), before_repos);
            after.sort_unstable();
            after.dedup();
            was.sort_unstable();
            if after != was {
                // Names, for the reason the workstream line above uses one.
                let names: Vec<String> = self.task_repos(project_id, id)?.into_iter().map(|r| r.name).collect();
                lead.push(if names.is_empty() { "repos cleared".to_string() } else { format!("repos: {}", names.join(", ")) });
            }
        }
        if planio != before_planio {
            lead.push(match planio {
                Some(n) => format!("planio #{n}"),
                None => "planio ref cleared".to_string(),
            });
        }
        let summary = patch.log.clone()
            .unwrap_or_else(|| describe_change(&before, &title, &body, status, priority, blocked_by, lead));
        self.write_event(before.project_id, Some(id), patch.actor, &kind, &summary)?;

        self.task(project_id, id)
    }

    /// A task's workstream id. A query of its own because `workstream_id` is kept out of
    /// `TASK_COLS` -- see migration 005 -- so no `Task` carries it.
    pub fn task_workstream(&self, project_id: i64, id: i64) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        Ok(self.conn.query_row(
            "SELECT workstream_id FROM tasks WHERE id = ?1 AND project_id = ?2",
            params![id, project_id], |r| r.get(0),
        ).optional()?.flatten())
    }

    pub fn tasks_blocked_by(&self, project_id: i64, id: i64) -> Result<Vec<Task>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE blocked_by = ?1 AND project_id = ?2 AND status != 'archived' ORDER BY updated_at DESC, id DESC"
        ))?;
        Ok(st.query_map(params![id, project_id], row_to_task)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One task with everything needed to resume it cold.
    pub fn task_detail(&self, project_id: i64, id: i64) -> Result<TaskDetail> {
        let task = self.task(project_id, id)?;
        let project = self.project(task.project_id)?;
        let blocker = match task.blocked_by { Some(b) => self.task_opt(project_id, b)?, None => None };
        Ok(TaskDetail {
            tags: self.task_tags(project_id, id)?,
            repos: self.task_repos(project_id, id)?,
            planio: self.task_planio(project_id, id)?,
            board_has_repos: self.repo_count(project_id)? > 0,
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

/// A Planio issue number is positive. Checked in core so both adapters refuse the same
/// values; each maps its own "clear it" spelling to `None` before this sees it.
fn check_planio(planio: Option<i64>) -> Result<()> {
    match planio {
        Some(n) if n <= 0 => Err(Error::InvalidValue {
            field: "planio",
            value: n.to_string(),
            valid: "a Planio issue number, e.g. 48213".into(),
        }),
        _ => Ok(()),
    }
}

/// Fallback event text when the caller gave no reason. States what changed, so the history
/// is at least factual -- but it is deliberately duller than a real `log`, because "what"
/// without "why" is the weaker half.
fn describe_change(
    before: &Task, title: &str, body: &str, status: Status, priority: Priority,
    blocked_by: Option<i64>, lead: Vec<String>,
) -> String {
    // First: changes resolved to names by the caller -- a workstream move, new repos, a
    // Planio ref. They are the most significant things that can happen to a task without its
    // status changing, and the ones a reader is least able to reconstruct later.
    let mut parts = lead;
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
