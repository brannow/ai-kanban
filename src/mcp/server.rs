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
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The literal an agent passes to search or list every board.
const ALL: &str = "all";

// There is deliberately no `forget` tool, and its absence is a decision rather than an
// oversight. `Store::forget_task` / `forget_note` destroy history permanently; that is a
// human act, exposed only over HTTP. An agent that can delete history is an agent that can
// cover its own tracks, on a board whose whole purpose is that the record survives. The
// agent's answer to a mistake stays `archive`, or an overwrite that keeps the old value.

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
                // Key and name matching lives in core, so the CLI and this agree on what
                // "ambiguous" means. Ambiguity is never resolved silently in either --
                // picking one would write to a board the caller did not mean, and nothing
                // downstream would surface it.
                match store.project_by_name_or_key(n) {
                    Ok(p) => Ok(Resolved { project: p, how: crate::core::project::Resolution::KnownPath, created: false }),
                    // A directory is the adapter's fallback, not core's: only a consumer
                    // with a filesystem to stand in has any use for it, and unlike the
                    // other two it may *create* a board.
                    Err(Error::ProjectNotFound { .. }) if PathBuf::from(n).is_dir() => {
                        store.resolve_project(&PathBuf::from(n))
                    }
                    Err(e) => Err(e),
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
    /// Narrow the board to one workstream -- a feature, an upgrade, a migration -- and
    /// work there. Unknown names start a new one. Pass "all" to widen back out.
    ///
    /// Tasks you file afterwards join this workstream automatically.
    pub workstream: Option<String>,
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
    /// Freeform labels, comma separated -- "in review", "waiting-on-vendor", "frontend".
    /// For the person's filtering: they carry no meaning you have to act on, and they are
    /// not shown on the board.
    pub tags: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct TaskUpdateParams {
    /// The task id, as shown on the board.
    pub task: i64,
    /// backlog, doing, blocked, done or archived.
    pub status: Option<String>,
    /// Move it to another workstream. Pass "" to make it general work with no workstream.
    ///
    /// Worth reaching for when a task turns out to belong to different work than the one
    /// you were in when you filed it -- tasks join the current workstream automatically,
    /// so that happens.
    pub workstream: Option<String>,
    pub priority: Option<String>,
    #[serde(rename = "type")]
    pub task_type: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Task id this one is waiting on. Pass 0 to clear it.
    pub blocked_by: Option<i64>,
    /// Replaces the labels, comma separated. Pass "" to clear them.
    pub tags: Option<String>,
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

// `vis = "pub"` so the tool list -- and therefore every generated schema -- is reachable
// from an integration test. See `tests/mcp_schema.rs`.
#[tool_router(vis = "pub")]
impl AiKanban {
    /// Show the board: what is in flight, what is blocked and why, and what happened
    /// recently. Call this when starting work on a project to find out where things stand.
    ///
    /// Pass `workstream` when the user says what you are working on ("we're doing the
    /// contact form now") -- the board narrows to it and tasks you file afterwards join it.
    ///
    /// `read_only_hint` is **false** because of that argument, and the honesty costs
    /// something worth paying for. A `board` call with a `workstream` is no longer a pure
    /// read: it records where work is happening, which every later `task_add` inherits.
    /// Annotating it read-only would be convenient and untrue, and a client that trusts the
    /// annotation is exactly the one that would be surprised. Plain `board()` writes
    /// nothing; the annotation cannot be conditional, so it describes the wider case.
    /// `idempotent_hint` still holds -- entering the same workstream twice is one state.
    #[tool(name = "board", annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true))]
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

        let pid = resolved.project.id;
        let before = store.current_workstream(pid).map_err(fail)?;
        let mut switched = None;
        match p.workstream.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            // "all" leaves the workstream rather than naming one, mirroring how `project`
            // and `include` already use the word. Without a way out, a board entered once
            // could never be widened again except by naming another workstream.
            Some(w) if w.eq_ignore_ascii_case(ALL) => {
                store.clear_current_workstream(pid).map_err(fail)?;
                q.workstream = None;
            }
            Some(w) => {
                let ws = store.ensure_workstream(pid, w).map_err(fail)?;
                if before.as_ref().map(|b| b.id) != Some(ws.id) {
                    store.set_current_workstream(pid, ws.id).map_err(fail)?;
                    switched = Some(ws.name.clone());
                }
                q.workstream = Some(ws.id);
            }
            // No argument: inherit whatever the board is already scoped to. This is what
            // makes the scope survive between calls and, more importantly, what lets the
            // SessionStart hook honour it -- the hook passes no arguments at all.
            None => q.workstream = before.as_ref().map(|b| b.id),
        }

        let snap = store.board(pid, &q).map_err(fail)?;
        let mut out = String::new();
        // Say the switch out loud. Entering by looking is convenient and has one sharp
        // edge: an agent that glances at an adjacent workstream has silently changed where
        // its next `task_add` lands. Unlike alias learning -- the precedent for
        // state-as-a-side-effect-of-use -- this does not converge, so it has to announce
        // itself rather than rely on the agent inferring it from a changed header.
        if let Some(name) = switched {
            out.push_str(&format!("Now working in: {name}. Tasks you file join it.\n\n"));
        }
        out.push_str(&render::board(&snap));
        Ok(out)
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
            tags: csv(&p.tags).unwrap_or_default(),
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
            // Empty string clears, matching how 0 clears `blocked_by` just above: an
            // optional JSON field cannot express "set this to null" on its own.
            workstream: match p.workstream.as_deref().map(str::trim) {
                None => None,
                Some("") => Some(None),
                Some(name) => Some(Some(
                    store.ensure_workstream(resolved.project.id, name).map_err(fail)?.id,
                )),
            },
            // Absent leaves them alone; "" clears them. Same three-way reading as
            // `workstream` above, so one shape means one thing across the whole tool.
            tags: p.tags.as_deref().map(|t| csv(&Some(t.to_string())).unwrap_or_default()),
            log: p.log.clone(),
            // Unguarded, deliberately -- see `TaskPatch::expected_version`.
            expected_version: None,
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
            // Unguarded, deliberately. Requiring a version here would make every update a
            // two-call sequence -- see `TaskPatch::expected_version`.
            expected_version: None,
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

        // Only for a single-project search. Cross-project hits come from repos whose
        // checkouts are mostly not on this machine, so the calibration in `staleness` would
        // decline to say anything for nearly all of them anyway -- and computing it per
        // project to reach that conclusion is work with no output.
        let missing = match project_id {
            Some(pid) => {
                let notes: Vec<i64> = result.hits.iter()
                    .filter(|h| h.kind == HitKind::Note).map(|h| h.id).collect();
                store.missing_subjects(pid, &notes).unwrap_or_default()
            }
            None => Default::default(),
        };
        Ok(render::recall(&result, cross, &missing))
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

/// Written out rather than left to the macro so the server can carry `instructions`.
///
/// Instructions are a **second adoption channel**, independent of hooks: the client receives
/// them at initialize and they reach the model without anything having to fire, be enabled,
/// or be chosen. If the hooks are ever disabled, this is what is left, so it says the same
/// thing in fewer words rather than describing the API.
#[tool_handler]
impl ServerHandler for AiKanban {
    /// Defined here purely to run every tool schema through `schema::split_type_unions` on
    /// the way out. `#[tool_handler]` generates `list_tools` only when the impl does not
    /// already have one, so writing it here replaces that generated body and leaves
    /// `call_tool` alone. The rest of this mirrors what the macro would have produced.
    ///
    /// Rewriting here rather than at the type level keeps the parameter structs honest: an
    /// `Option<String>` stays an `Option<String>`, and the shape other clients need is a
    /// serialisation concern handled at the boundary where it belongs.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, ErrorData> {
        let supports_cache_hints = context.protocol_version().is_some_and(|version| {
            version >= rmcp::model::ProtocolVersion::V_2026_07_28
        });
        let tools = crate::mcp::schema::portable_tools(Self::tool_router().list_all());
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }

    fn get_info(&self) -> ServerInfo {
        // Both ServerInfo and Implementation are #[non_exhaustive], so they are built
        // through their constructors and then adjusted.
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::from_build_env();
        info.server_info.name = "ai-kanban".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "A kanban board that is this project's memory across sessions.\n\n\
             Call `board` when you start work on a project to see what is in flight, what is \
             blocked and why, and what happened recently.\n\n\
             File a task as soon as you notice something worth doing -- an unrelated bug, a \
             TODO, a side quest -- rather than carrying it or dropping it. When you move a \
             task, say why: what changed is recoverable from the diff, why it changed is not. \
             Record what you work out about the codebase as a note, with the files it \
             concerns. Before debugging something that feels familiar, try `recall` -- it \
             searches other projects too."
                .into(),
        );
        info
    }
}
