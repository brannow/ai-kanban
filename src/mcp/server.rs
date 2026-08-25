//! The MCP tool surface.
//!
//! # The design law this file follows
//!
//! > A tool is not a thin wrapper for an API call. It is a refined tool, like an
//! > application for a human. The test: **did the agent need knowledge about the system's
//! > internals in order to use it?** If yes, the tool is not good.
//!
//! In practice that means:
//!
//!  * **One call per intent.** No "create then fetch to confirm" pairs.
//!  * **Prose out, not JSON.** The agent is not a REST client.
//!  * **Self-contained responses.** A bad response causes three follow-up calls; a good one
//!    causes none. That is why every mutation returns a board rather than an "ok".
//!  * **Errors self-correct.** Not-found shows what does exist. An invalid value lists the
//!    valid ones.
//!  * **Every argument we don't bother the agent with is a win.** Only `title`, `query` and
//!    the ids are ever required.
//!
//! # Why `project` is optional everywhere
//!
//! It resolves from where the caller actually is (`CLAUDE_PROJECT_DIR`, then cwd). Asking a
//! model to supply a project identifier looks harmless and is not: generated identifiers
//! are non-deterministic -- `ai-kanban` today, `ai_kanban` tomorrow -- and the board
//! silently forks into two half-memories. Paths resolve the same way every time.
//! The parameter stays available for deliberate cross-project queries.

use crate::core::model::*;
use crate::core::note::{NoteDraft, NotePatch};
use crate::core::project::Resolved;
use crate::core::recall::{RecallQuery, DEFAULT_LIMIT};
use crate::core::task::{TaskDraft, TaskPatch};
use crate::core::{Error, Store};
use crate::render;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The literal an agent passes to search or list every board.
const ALL: &str = "all";

pub struct AiKanban {
    store: Arc<Mutex<Store>>,
    /// Captured once at startup. The server is spawned inside the project by the host, so
    /// this is the project context for the whole process.
    cwd: PathBuf,
}

impl AiKanban {
    pub fn new(store: Store) -> Self {
        Self { store: Arc::new(Mutex::new(store)), cwd: project_dir() }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolves the board this call is about.
    ///
    /// A named project is matched against key, name, and path -- an agent that saw
    /// "Board: ai-kanban" in an earlier response should be able to pass exactly that back,
    /// which is the "output of one tool is valid input to another" rule.
    fn resolve(&self, store: &Store, name: Option<&str>) -> Result<Resolved, Error> {
        match name {
            None => store.resolve_project(&self.cwd),
            Some(n) => {
                let n = n.trim();
                if let Some(p) = store.project_by_key(n)? {
                    return Ok(Resolved { project: p, how: crate::core::project::Resolution::KnownPath, created: false });
                }
                let all = store.all_projects()?;
                let matches: Vec<&Project> = all.iter().filter(|p| p.name.eq_ignore_ascii_case(n)).collect();
                match matches.len() {
                    1 => Ok(Resolved { project: matches[0].clone(), how: crate::core::project::Resolution::KnownPath, created: false }),
                    // Ambiguity is never resolved silently -- picking one would write to a
                    // board the agent did not mean, and nothing would surface it.
                    n_matches if n_matches > 1 => Err(Error::AmbiguousProject {
                        query: n.to_string(),
                        candidates: matches.iter().map(|p| format!("{} ({})", p.name, p.key)).collect(),
                    }),
                    _ => {
                        let path = PathBuf::from(n);
                        if path.is_dir() {
                            return store.resolve_project(&path);
                        }
                        Err(Error::ProjectNotFound {
                            query: n.to_string(),
                            existing: all.iter().map(|p| p.name.clone()).collect(),
                        })
                    }
                }
            }
        }
    }
}

/// `CLAUDE_PROJECT_DIR` first, process cwd second.
///
/// Deliberately *not* `roots/list`: SEP-2577 (Final) deprecates Roots and names environment
/// variables among its replacements. Roots still works today, so it is fine to consult
/// opportunistically later -- but building the primary path on a deprecated capability
/// would be designing toward a removal date.
fn project_dir() -> PathBuf {
    std::env::var_os("CLAUDE_PROJECT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Core errors become prose, not error codes. An agent that receives "invalid status" and
/// no list of valid ones has to guess or ask; both cost a round trip.
fn fail(e: Error) -> ErrorData {
    ErrorData::invalid_params(render::error(&e), None)
}

/// Parses an enum, or fails with the full list of valid values.
fn parse_enum<T, F>(field: &'static str, value: &Option<String>, f: F) -> Result<Option<T>, ErrorData>
where F: Fn(&str) -> Result<T, String> {
    match value {
        None => Ok(None),
        Some(v) => match f(v.trim()) {
            Ok(x) => Ok(Some(x)),
            Err(valid) => Err(fail(Error::InvalidValue { field, value: v.clone(), valid })),
        },
    }
}

fn csv(s: &Option<String>) -> Option<Vec<String>> {
    s.as_ref().map(|v| v.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct BoardParams {
    /// Which board. Omit for the current project. Pass "all" for a summary of every board.
    pub project: Option<String>,
    /// Restrict to statuses, comma separated: backlog, doing, blocked, done, archived.
    /// Omit for open work only.
    pub status: Option<String>,
    /// Show more than the default. Pass "all" to lift the cap on listed tasks.
    pub include: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct TaskAddParams {
    /// What needs doing, in one line.
    pub title: String,
    /// Detail worth keeping: what you saw, where, why it matters.
    pub body: Option<String>,
    /// task, bug, idea or chore. Defaults to task.
    #[serde(rename = "type")]
    pub task_type: Option<String>,
    /// low, normal, high or urgent. Defaults to normal.
    pub priority: Option<String>,
    /// backlog, doing or blocked. Defaults to backlog.
    pub status: Option<String>,
    /// Pass "user" when filing something the user asked for. Defaults to "agent".
    pub origin: Option<String>,
    /// Task id this one is waiting on.
    pub blocked_by: Option<i64>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct TaskUpdateParams {
    /// The task id, as shown on the board.
    pub task: i64,
    /// backlog, doing, blocked, done or archived.
    pub status: Option<String>,
    pub priority: Option<String>,
    #[serde(rename = "type")]
    pub task_type: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Task id this one is waiting on. Pass 0 to clear it.
    pub blocked_by: Option<i64>,
    /// Why this changed. This is the part worth reading in six months -- record the
    /// reasoning, not the fact that something moved.
    pub log: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct TaskShowParams {
    pub task: i64,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct NoteAddParams {
    /// The claim, in one line: "auth middleware rewrites redirects".
    pub title: String,
    /// The detail that makes it useful later.
    pub body: Option<String>,
    /// Comma separated.
    pub tags: Option<String>,
    /// Files this is about, comma separated. Worth passing: it is what will let this note
    /// surface when someone opens those files.
    pub paths: Option<String>,
    /// Task this was learned while working on.
    pub task: Option<i64>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct NoteUpdateParams {
    pub note: i64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<String>,
    pub paths: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct RecallParams {
    /// What you want to know about. Plain words -- no search syntax needed.
    pub query: String,
    /// Omit for this project. Pass "all" to search every board you have ever worked on.
    pub project: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct LogParams {
    /// What happened or what was decided, and why.
    pub body: String,
    /// Attach to a task. Omit for project-level history.
    pub task: Option<i64>,
    pub project: Option<String>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl AiKanban {
    /// Show the board: what is in flight, what is blocked and why, and what happened
    /// recently. Call this when starting work on a project to find out where things stand.
    #[tool(name = "board", annotations(read_only_hint = true, idempotent_hint = true))]
    fn board(&self, Parameters(p): Parameters<BoardParams>) -> Result<String, ErrorData> {
        let store = self.store();

        // "all" is a summary of every board, never a merged task list: every open task
        // across every project is a pile, not a board.
        if p.project.as_deref().map(|s| s.eq_ignore_ascii_case(ALL)).unwrap_or(false) {
            let (sums, total) = store.project_summaries(Store::SUMMARY_LIMIT).map_err(fail)?;
            return Ok(render::project_summaries(&sums, total, crate::core::now()));
        }

        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;
        let mut q = BoardQuery::board();
        if let Some(s) = &p.status {
            let mut statuses = Vec::new();
            for part in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
                statuses.push(Status::parse(part).map_err(|valid| {
                    fail(Error::InvalidValue { field: "status", value: part.to_string(), valid })
                })?);
            }
            q.status = statuses;
        }
        if p.include.as_deref().map(|s| s.eq_ignore_ascii_case(ALL)).unwrap_or(false) {
            q.limit = usize::MAX;
        }

        let snap = store.board(resolved.project.id, &q).map_err(fail)?;
        Ok(render::board(&snap))
    }

    /// File a task. Use this the moment you notice something worth doing -- an unrelated
    /// bug, a TODO, a side quest -- so it survives the end of this session. One call, no
    /// context switch: it returns the board so you can see it landed and keep going.
    #[tool(name = "task_add", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false))]
    fn task_add(&self, Parameters(p): Parameters<TaskAddParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;

        let draft = TaskDraft {
            title: p.title.clone(),
            body: p.body.clone().unwrap_or_default(),
            task_type: parse_enum("type", &p.task_type, TaskType::parse)?.unwrap_or_default(),
            origin: parse_enum("origin", &p.origin, Origin::parse)?.unwrap_or_default(),
            priority: parse_enum("priority", &p.priority, Priority::parse)?.unwrap_or_default(),
            status: parse_enum("status", &p.status, Status::parse)?.unwrap_or_default(),
            blocked_by: p.blocked_by.filter(|b| *b > 0),
        };
        let task = store.create_task(resolved.project.id, draft).map_err(fail)?;
        let snap = store.board_after_mutation(resolved.project.id, task.id).map_err(fail)?;
        Ok(render::board(&snap))
    }

    /// Move a task and record why, in one call. The `log` argument is the important half:
    /// a board that says what changed but not why is a worse memory than no board at all.
    #[tool(name = "task_update", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false))]
    fn task_update(&self, Parameters(p): Parameters<TaskUpdateParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;

        let patch = TaskPatch {
            title: p.title.clone(),
            body: p.body.clone(),
            status: parse_enum("status", &p.status, Status::parse)?,
            priority: parse_enum("priority", &p.priority, Priority::parse)?,
            task_type: parse_enum("type", &p.task_type, TaskType::parse)?,
            // 0 is the clear signal: JSON has no way to say "set this to null" that
            // survives an optional field, and inventing a magic string would be worse.
            blocked_by: p.blocked_by.map(|b| if b > 0 { Some(b) } else { None }),
            log: p.log.clone(),
            actor: Actor::Agent,
        };
        store.update_task(resolved.project.id, p.task, patch).map_err(fail)?;
        let snap = store.board_after_mutation(resolved.project.id, p.task).map_err(fail)?;
        Ok(render::board(&snap))
    }

    /// Everything about one task: its full history, what blocks it, and the notes attached
    /// to it. Use this to resume a specific piece of work you have not touched in a while.
    #[tool(name = "task_show", annotations(read_only_hint = true, idempotent_hint = true))]
    fn task_show(&self, Parameters(p): Parameters<TaskShowParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;
        let detail = store.task_detail(resolved.project.id, p.task).map_err(fail)?;
        Ok(render::task_detail(&detail))
    }

    /// Record something durable you learned about this codebase -- how a subsystem behaves,
    /// why something is the way it is, a trap to avoid. Notes outlive the task that
    /// produced them, so write the claim, not the narrative.
    #[tool(name = "note_add", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false))]
    fn note_add(&self, Parameters(p): Parameters<NoteAddParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;

        let draft = NoteDraft {
            title: p.title.clone(),
            body: p.body.clone().unwrap_or_default(),
            tags: csv(&p.tags).unwrap_or_default(),
            paths: csv(&p.paths).unwrap_or_default(),
            task_id: p.task,
        };
        let note = store.create_note(resolved.project.id, draft, Actor::Agent).map_err(fail)?;
        Ok(format!("Board: {}\n\nnote #{} \"{}\" recorded.\n", resolved.project.name, note.id, note.title))
    }

    /// Correct a note that is no longer true. Do this as soon as you find one wrong:
    /// confidently wrong memory is worse than none. The previous version is kept in the
    /// project history, so nothing is lost by fixing it.
    #[tool(name = "note_update", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false))]
    fn note_update(&self, Parameters(p): Parameters<NoteUpdateParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;

        let patch = NotePatch {
            title: p.title.clone(),
            body: p.body.clone(),
            tags: csv(&p.tags),
            paths: csv(&p.paths),
            actor: Actor::Agent,
        };
        let note = store.update_note(resolved.project.id, p.note, patch).map_err(fail)?;
        Ok(format!("Board: {}\n\nnote #{} \"{}\" updated. The previous version is in the project history.\n",
            resolved.project.name, note.id, note.title))
    }

    /// Search everything remembered about a topic -- notes, tasks and the reasoning
    /// recorded against them. Use it before debugging something that feels familiar, and
    /// pass project "all" to check whether you hit the same problem on another codebase.
    #[tool(name = "recall", annotations(read_only_hint = true, idempotent_hint = true))]
    fn recall(&self, Parameters(p): Parameters<RecallParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let cross = p.project.as_deref().map(|s| s.eq_ignore_ascii_case(ALL)).unwrap_or(false);

        let project_id = if cross {
            None
        } else {
            Some(self.resolve(&store, p.project.as_deref()).map_err(fail)?.project.id)
        };
        let result = store.recall(&RecallQuery {
            text: &p.query,
            project_id,
            limit: p.limit.map(|l| l.clamp(1, 50) as usize).unwrap_or(DEFAULT_LIMIT),
        }).map_err(fail)?;
        Ok(render::recall(&result, cross))
    }

    /// Record what happened or what was decided. Use it for decisions and session summaries
    /// -- the things a future session would otherwise have to reconstruct. Attach it to a
    /// task with `task`, or omit that for project-level history. Either way it shows up in
    /// the board's recent section.
    #[tool(name = "log", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false))]
    fn log(&self, Parameters(p): Parameters<LogParams>) -> Result<String, ErrorData> {
        let store = self.store();
        let resolved = self.resolve(&store, p.project.as_deref()).map_err(fail)?;
        store.log_on(resolved.project.id, p.task, Actor::Agent, &p.body).map_err(fail)?;
        Ok(format!("Board: {}\n\nrecorded.\n", resolved.project.name))
    }
}
