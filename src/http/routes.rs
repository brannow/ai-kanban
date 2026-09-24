//! Request handlers.
//!
//! Every write is `actor: user` / `origin: user`, hardcoded and never taken from the client.
//! Not because "the web UI is a human" -- because `tasks.origin` and `events.actor` are the
//! instrumentation for priority #2 (does the agent file work unprompted), and `docs/adoption.md`
//! already describes that number as a floor rather than a measurement. A client-supplied
//! actor lets the floor be inflated, which makes the one number the project judges itself by
//! worthless.

use super::{Api, ApiError, ApiResult};
use crate::core::model::*;
use crate::core::note::{NoteDraft, NotePatch};
use crate::core::page::Cursor;
use crate::core::recall::RecallQuery;
use crate::core::task::{TaskDraft, TaskPatch};
use crate::core::{Error, Store};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

/// Rows returned per request. An order of magnitude above the agent's caps, because the
/// constraint here is a scrollbar rather than a token budget -- and deliberately a separate
/// number, so nobody "reconciles" the two and quietly makes the board expensive.
const PAGE: usize = 50;

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

/// Resolves `{p}`, which is an id or a project key.
///
/// Names are not accepted. They are not unique -- two clones of different repos can both be
/// called `api` -- and silently picking one would show a board the caller did not ask for.
fn resolve(store: &Store, p: &str) -> Result<Project, Error> {
    if let Ok(id) = p.parse::<i64>() {
        return store.project(id);
    }
    match store.project_by_key(p)? {
        Some(project) => Ok(project),
        None => Err(Error::ProjectNotFound {
            query: p.to_string(),
            existing: store.all_projects()?.iter().map(|x| x.key.clone()).collect(),
        }),
    }
}

/// Cursors are opaque: the client echoes back what it was given and never parses one.
/// Encoding both sort fields keeps a third one from becoming a breaking change.
fn encode_cursor(c: Cursor) -> String {
    format!("{}.{}", c.ts, c.id)
}

fn decode_cursor(s: &Option<String>) -> Result<Option<Cursor>, Error> {
    let Some(s) = s.as_deref().filter(|s| !s.is_empty()) else { return Ok(None) };
    let bad = || Error::InvalidValue {
        field: "cursor",
        value: s.to_string(),
        valid: "a cursor from a previous response".into(),
    };
    let (ts, id) = s.split_once('.').ok_or_else(bad)?;
    Ok(Some(Cursor {
        ts: ts.parse().map_err(|_| bad())?,
        id: id.parse().map_err(|_| bad())?,
    }))
}

fn parse_status_list(s: &Option<String>) -> Result<Vec<Status>, Error> {
    let Some(s) = s.as_deref().filter(|s| !s.is_empty()) else { return Ok(vec![]) };
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            Status::parse(t).map_err(|valid| Error::InvalidValue {
                field: "status",
                value: t.to_string(),
                valid,
            })
        })
        .collect()
}

fn enum_field<T: Copy + std::fmt::Display>(
    field: &'static str, value: &Option<String>, parse: impl Fn(&str) -> Result<T, String>,
) -> Result<Option<T>, Error> {
    match value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => parse(v)
            .map(Some)
            .map_err(|valid| Error::InvalidValue { field, value: v.to_string(), valid }),
    }
}

/// The version the client believes it is changing, from `If-Match`.
///
/// Required on writes. A browser form open for two minutes while an agent moves the same task
/// is the case the guard exists for, and a client that forgets the header would silently get
/// last-write-wins -- so its absence is an error rather than a fallback.
fn if_match(headers: &HeaderMap) -> Result<i64, Error> {
    headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().trim_matches('"'))
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| Error::InvalidValue {
            field: "If-Match",
            value: "missing".into(),
            valid: "the `version` from the entity you read".into(),
        })
}

fn etag(version: i64) -> [(header::HeaderName, String); 1] {
    [(header::ETAG, format!("\"{version}\""))]
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

/// Enum values and versions, so the UI never hardcodes a status list.
///
/// The design law applied to the second consumer: a page that hardcodes
/// `["backlog","doing","blocked"]` needs internal knowledge of the schema and drifts silently
/// the day a status is added. Values arrive in display order (`Status::column_rank`,
/// `Priority::rank`) so the client renders columns left to right without knowing the rule.
///
/// Carries no cursor on purpose -- this document is cacheable for the life of the process,
/// and mixing a constantly-changing value into it would make a client refetch the enum table
/// forever.
pub async fn meta(State(api): State<Api>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let mut statuses: Vec<Status> = Status::ALL.to_vec();
    statuses.sort_by_key(|s| s.column_rank());
    let mut priorities: Vec<Priority> = Priority::ALL.to_vec();
    priorities.sort_by_key(|p| p.rank());

    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "schema_version": store.schema_version()?,
        "statuses": statuses.iter().map(|s| json!({
            "value": s.as_str(), "open": s.is_open()
        })).collect::<Vec<_>>(),
        "open_statuses": statuses.iter().filter(|s| s.is_open()).map(|s| s.as_str()).collect::<Vec<_>>(),
        "priorities": priorities.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "types": TaskType::ALL.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
        "actors": Actor::ALL.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        // Where an external ref links to: a template whose `{ref}` the UI fills in, e.g.
        // `https://frs.plan.io/issues/{ref}` or `https://acme.atlassian.net/browse/{ref}`.
        // A setting rather than a constant: the store knows refs, not which tracker they
        // are in. Unset, the UI shows the ref without a link.
        "ref_url": std::env::var("AI_KANBAN_REF_URL").ok()
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty()),
    })))
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

pub async fn projects(State(api): State<Api>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    // No cap: the agent's summary is capped because it pays per token; a person picking a
    // board from a dropdown wants all of them.
    let (summaries, total) = store.project_summaries(usize::MAX)?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "cursor": store.change_cursor()?,
        "projects": summaries,
        "total": total,
    })))
}

#[derive(Debug, Deserialize, Default)]
pub struct BoardBody {
    pub name: Option<String>,
}

/// Creates a board by name -- a tracker's project, a customer. A person's act, like adding
/// repos: the agent reaches a board through its repos and never creates one by name, because
/// a name an agent generates is how one project quietly becomes two.
pub async fn create_board(
    State(api): State<Api>, Json(b): Json<BoardBody>,
) -> ApiResult<impl IntoResponse> {
    let store = api.store();
    let project = store.create_board(b.name.as_deref().unwrap_or_default())?;
    Ok((StatusCode::CREATED, Json(json!({ "project": project }))))
}

pub async fn project(State(api): State<Api>, Path(p): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "project": project,
        // From `project_paths` rather than from the `path_learned` events: this is the data,
        // those are the record of it being written. A person checking whether a board has
        // split wants the former.
        "paths": store.project_paths(project.id)?,
        "counts": store.status_counts(project.id)?,
        "profiles": profiles_json(&store, project.id)?,
    })))
}

// ---------------------------------------------------------------------------
// Board
// ---------------------------------------------------------------------------

/// One request paints the whole board; each column then pages independently against `/tasks`.
///
/// Not a `BoardSnapshot`: the agent's snapshot is a capped list with an `omitted` count,
/// which is right for something that must fit a token budget and wrong for columns you
/// scroll.
pub async fn board(State(api): State<Api>, Path(p): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;

    // One read transaction for the whole board. `counts` and the per-column pages are
    // separate queries, and an agent writing between them would render "backlog (12)" above
    // thirteen cards. A deferred read costs nothing and removes the class.
    let tx = store.conn.unchecked_transaction()?;

    // The human board follows the SAME current workstream the agent's does. One board, one
    // active workstream, shared by the person and their agents -- a UI that showed a
    // different slice than the agent is working in would make the two disagree about what
    // "the board" means, which is the confusion this whole feature removes.
    let current = store.current_workstream(project.id)?;
    let ws = current.as_ref().map(|w| w.id);

    let counts = store.status_counts_in(project.id, ws)?;
    // Column order, not the agent's `board_rank` -- see `Status::column_rank`.
    let mut statuses: Vec<Status> = Status::ALL.to_vec();
    statuses.sort_by_key(|s| s.column_rank());

    let mut columns = Vec::new();
    // Kept alongside the JSON so the blocker lookup below is one query for the whole board
    // rather than one per column.
    let mut listed: Vec<Task> = Vec::new();
    for s in statuses {
        let page = store.tasks_page_in(project.id, &[s], None, PAGE, ws)?;
        listed.extend(page.items.iter().cloned());
        columns.push(json!({
            "status": s.as_str(),
            "total": counts.iter().find(|(k, _)| *k == s).map(|(_, n)| *n).unwrap_or(0),
            "tasks": page.items,
            "next": page.next.map(encode_cursor),
        }));
    }
    // A card carries only its blocker's id, so without this the web board says "blocked by
    // #16" forever -- including after #16 is done. Same defect as the agent board had, and
    // fixing one and not the other is how the two views start disagreeing.
    let blockers = store.blocker_status(project.id, &listed)?;
    let tags = store.tags_for(project.id, &listed)?;
    let links = store.links_for(project.id, &listed)?;
    let repos = store.repo_summaries(project.id)?;
    let cursor = store.change_cursor()?;
    tx.commit()?;

    Ok(Json(json!({
        "now": crate::core::now(),
        "cursor": cursor,
        "project": project,
        "counts": counts.iter().map(|(s, n)| json!({ "status": s.as_str(), "count": n })).collect::<Vec<_>>(),
        "columns": columns,
        "blocker_status": blockers.iter()
            .map(|(id, s)| json!({ "id": id, "status": s.as_str() }))
            .collect::<Vec<_>>(),
        // Keyed by task id rather than inlined on the row: `tags` is out of `TASK_COLS`
        // (migration 006), so the listed `Task` values do not carry it. Only tasks that
        // actually have tags appear here.
        "task_tags": tags.iter().map(|(id, t)| (id.to_string(), json!(t)))
            .collect::<serde_json::Map<String, serde_json::Value>>(),
        // Same shape and the same reason as `task_tags`: neither the repo links nor
        // `external_ref` is in `TASK_COLS` (migrations 007, 012).
        "task_links": links_json(&links),
        // Every repo on the board, so the cards, the pickers and the repos menu all come
        // from this one read.
        "repos": repos,
        // Both halves are needed by the UI: `workstream` is what the board is scoped to and
        // what a new task will join, `workstreams` is everything selectable. Sent even when
        // nothing is scoped, so the control can offer the list without a second request.
        "workstream": current,
        "workstreams": store.all_open_workstreams(project.id)?,
    })))
}

// ---------------------------------------------------------------------------
// Workstream
// ---------------------------------------------------------------------------

/// Resolve a workstream by name, **without creating one**.
///
/// The human API deliberately cannot create workstreams; only the agent can, as a side
/// effect of being told what it is working on. A person picks from what exists.
///
/// Enforced here rather than by omitting a button, because a UI-only restriction is
/// bypassed by any direct call or a stale page still holding the old form. The error lists
/// the existing names, following the same self-correcting shape every other invalid value
/// in this API uses.
fn existing_workstream(store: &Store, project_id: i64, name: &str) -> ApiResult<i64> {
    if let Some(w) = store.workstream_by_name(project_id, name)? {
        return Ok(w.id);
    }
    let mut names: Vec<String> = store.all_open_workstreams(project_id)?
        .into_iter().map(|w| w.workstream.name).collect();
    names.sort();
    Err(ApiError(Error::InvalidValue {
        field: "workstream",
        value: name.to_string(),
        valid: if names.is_empty() {
            "no workstreams on this board yet -- an agent starts one when you tell it what \
             you are working on".to_string()
        } else {
            names.join(", ")
        },
    }))
}

#[derive(Debug, Deserialize, Default)]
pub struct WorkstreamBody {
    /// An existing workstream to work in. `null` or absent widens back out to the whole
    /// board. Unknown names are refused, not created -- see `existing_workstream`.
    pub name: Option<String>,
}

/// Set (or clear) the board's current workstream. The name must already exist.
///
/// A `PUT` because it is idempotent and replaces one value -- and it exists on the human
/// side even though the agent surface deliberately has no such tool. The two consumers get
/// different affordances on purpose: an agent enters a workstream as a side effect of
/// asking to see it, because an extra call is one it would skip. A person clicking a
/// control is already stating intent, and hiding that behind a side effect would make the
/// scope change something they did not know they did.
pub async fn set_workstream(
    State(api): State<Api>, Path(p): Path<String>, Json(b): Json<WorkstreamBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    match b.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => {
            let id = existing_workstream(&store, project.id, name)?;
            store.set_current_workstream(project.id, id)?;
            Ok(Json(json!({ "workstream": store.workstream(project.id, id)? })))
        }
        None => {
            store.clear_current_workstream(project.id)?;
            Ok(Json(json!({ "workstream": serde_json::Value::Null })))
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct PageParams {
    pub status: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

/// Scoped to the project's current workstream, like `board` is.
///
/// This is the endpoint a board column pages against, so an unscoped page two would put more
/// cards in a column than its own header claims -- `counts` comes from `status_counts_in`,
/// which is already workstream-scoped. The scope is read here rather than accepted as a
/// query parameter on purpose: it can change between the board load and the "load more"
/// click, and a client replaying a stale name would silently mix two slices of the board.
pub async fn tasks(
    State(api): State<Api>, Path(p): Path<String>, Query(q): Query<PageParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let ws = store.current_workstream(project.id)?.map(|w| w.id);
    let page = store.tasks_page_in(
        project.id,
        &parse_status_list(&q.status)?,
        decode_cursor(&q.cursor)?,
        q.limit.unwrap_or(PAGE).min(200),
        ws,
    )?;
    // Appended pages need their tags too, or an expanded column shows labels on the first
    // 50 cards and none after -- which reads as "those tasks have no tags".
    let tags = store.tags_for(project.id, &page.items)?;
    let links = store.links_for(project.id, &page.items)?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "tasks": page.items,
        "task_tags": tags.iter().map(|(id, t)| (id.to_string(), json!(t)))
            .collect::<serde_json::Map<String, serde_json::Value>>(),
        "task_links": links_json(&links),
        "next": page.next.map(encode_cursor),
    })))
}

pub async fn task(
    State(api): State<Api>, Path((p, t)): Path<(String, i64)>,
) -> ApiResult<impl IntoResponse> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let detail = store.task_detail(project.id, t)?;
    let version = detail.task.version;
    Ok((etag(version), Json(json!({
        "now": detail.now,
        "task": detail.task,
        // Same reason as `workstream` below: out of `TASK_COLS`, so no `Task` carries it.
        "tags": detail.tags,
        "repos": detail.repos,
        "external_ref": detail.external_ref,
        "blocker": detail.blocker,
        "blocking": detail.blocking,
        "notes": detail.notes,
        "events": detail.events,
        // Which "open in …" buttons the panel shows.
        "profiles": profiles_json(&store, project.id)?,
        "project": detail.project,
        // Looked up rather than read off the task: `workstream_id` is kept out of
        // `TASK_COLS` (see migration 005), so no `Task` carries it. The panel needs the
        // name to preselect the move control.
        "workstream": match store.task_workstream(project.id, t)? {
            Some(w) => json!(store.workstream(project.id, w)?),
            None => serde_json::Value::Null,
        },
    }))))
}

#[derive(Debug, Deserialize, Default)]
pub struct TaskBody {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    #[serde(rename = "type")]
    pub task_type: Option<String>,
    /// `Some(None)` clears the blocker; omitted leaves it alone.
    pub blocked_by: Option<Option<i64>>,
    /// Freeform labels. Replaces the whole set on update; omitted leaves it alone.
    pub tags: Option<Vec<String>>,
    /// Repo ids on this board. Replaces the whole set on update; `[]` clears; omitted leaves
    /// it alone.
    pub repos: Option<Vec<i64>>,
    /// The issue in an outside tracker. `""` clears it on update -- the same spelling the
    /// agent uses, because a JSON `null` in an optional field reads as "absent" once
    /// deserialized.
    pub external_ref: Option<String>,
    /// The *why*. Recorded as the event body -- the field that makes history worth reading.
    pub log: Option<String>,
    /// Which **existing** workstream this task joins, by name. Omitted means "inherit the
    /// board's current one", which is what the agent does. Present-and-empty files it
    /// unscoped. An unknown name is an error listing the ones that exist.
    ///
    /// This override exists because a person can see the field and an agent cannot afford
    /// one: filing is the call an agent under pressure skips, so its workstream is
    /// inherited silently, while a human filling in a form is choosing.
    pub workstream: Option<String>,
}

pub async fn create_task(
    State(api): State<Api>, Path(p): Path<String>, Json(b): Json<TaskBody>,
) -> ApiResult<impl IntoResponse> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let title = b.title.clone().unwrap_or_default();
    if title.trim().is_empty() {
        return Err(ApiError(Error::InvalidValue {
            field: "title",
            value: title,
            valid: "a non-empty title".into(),
        }));
    }
    let draft = TaskDraft {
        title,
        body: b.body.clone().unwrap_or_default(),
        task_type: enum_field("type", &b.task_type, TaskType::parse)?.unwrap_or_default(),
        priority: enum_field("priority", &b.priority, Priority::parse)?.unwrap_or_default(),
        status: enum_field("status", &b.status, Status::parse)?.unwrap_or_default(),
        // Hardcoded. See the module header.
        origin: Origin::User,
        blocked_by: b.blocked_by.flatten(),
        tags: b.tags.clone().unwrap_or_default(),
        repos: b.repos.clone().unwrap_or_default(),
        external_ref: b.external_ref.clone(),
    };
    let task = match b.workstream.as_ref().map(|w| w.trim()) {
        // Absent: inherit whatever the board is scoped to, exactly as `task_add` does.
        None => store.create_task(project.id, draft)?,
        // Present but empty: an explicit "no workstream", which is the only way a person
        // can file general project work while the board is scoped to something.
        Some("") => store.create_task_in(project.id, draft, None)?,
        Some(name) => {
            let id = existing_workstream(&store, project.id, name)?;
            store.create_task_in(project.id, draft, Some(id))?
        }
    };
    let version = task.version;
    Ok((StatusCode::CREATED, etag(version), Json(json!({ "task": task }))))
}

pub async fn update_task(
    State(api): State<Api>, Path((p, t)): Path<(String, i64)>, headers: HeaderMap,
    Json(b): Json<TaskBody>,
) -> ApiResult<impl IntoResponse> {
    let expected = if_match(&headers)?;
    let store = api.store();
    let project = resolve(&store, &p)?;

    // Same three-way reading as on create, so one field means one thing across both verbs:
    // absent leaves it alone, "" makes it general work, a name moves it there.
    let workstream = match b.workstream.as_ref().map(|w| w.trim()) {
        None => None,
        Some("") => Some(None),
        Some(name) => Some(Some(existing_workstream(&store, project.id, name)?)),
    };
    let patch = TaskPatch {
        title: b.title.clone(),
        body: b.body.clone(),
        status: enum_field("status", &b.status, Status::parse)?,
        priority: enum_field("priority", &b.priority, Priority::parse)?,
        task_type: enum_field("type", &b.task_type, TaskType::parse)?,
        blocked_by: b.blocked_by,
        workstream,
        tags: b.tags.clone(),
        repos: b.repos.clone(),
        external_ref: b.external_ref.clone().map(Some),
        log: b.log.clone(),
        actor: Actor::User,
        expected_version: Some(expected),
    };
    let task = store.update_task(project.id, t, patch)?;
    let version = task.version;
    Ok((etag(version), Json(json!({ "task": task }))))
}

#[derive(Debug, Deserialize, Default)]
pub struct MoveBody {
    /// The board to move to, by id or key -- the same forms `{p}` accepts.
    pub to: Option<serde_json::Value>,
    pub log: Option<String>,
}

/// Moves a task to another board, keeping its id and history. An action of its own rather
/// than a `PATCH` field, because it changes the URL the task lives at.
///
/// `If-Match` required, like any other write to a task: a form held open while an agent moves
/// the same task is the lost update the guard exists for.
pub async fn move_task(
    State(api): State<Api>, Path((p, t)): Path<(String, i64)>, headers: HeaderMap,
    Json(b): Json<MoveBody>,
) -> ApiResult<impl IntoResponse> {
    let expected = if_match(&headers)?;
    let store = api.store();
    let project = resolve(&store, &p)?;
    let to = match &b.to {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => {
            return Err(ApiError(Error::InvalidValue {
                field: "to",
                value: String::new(),
                valid: "the id or key of the board to move to".into(),
            }))
        }
    };
    let target = resolve(&store, &to)?;
    let task = store.move_task(project.id, t, target.id, Actor::User, b.log.as_deref(), Some(expected))?;
    let version = task.version;
    Ok((etag(version), Json(json!({ "task": task, "project": target }))))
}

/// Starts work on a ticket: Claude Code in a new Ghostty tab (a window when Ghostty is not
/// open) in the ticket's first repo, with the others added, primed with the ticket
/// (`render::start_prompt`).
///
/// A ticket with no repo is refused -- the session has to start in a checkout, and guessing
/// one would start it in the wrong place. So is a profile the board does not allow. The launch
/// is recorded on the ticket, which is also what lets a live page see it happened.
pub async fn start_task(
    State(api): State<Api>, Path((p, t)): Path<(String, i64)>, headers: HeaderMap,
    Json(b): Json<StartBody>,
) -> ApiResult<Json<serde_json::Value>> {
    same_origin(&headers)?;
    let profile = super::launch::Profile::parse(b.profile.as_deref())?;
    let store = api.store();
    let project = resolve(&store, &p)?;
    if store.denied_profiles(project.id)?.iter().any(|d| d == profile.label()) {
        return Err(ApiError(Error::Forbidden(format!(
            "board {} does not allow {} sessions -- it can be allowed in the board's repos menu",
            project.name, profile.label()
        ))));
    }
    let detail = store.task_detail(project.id, t)?;
    // Only work nobody has started. A ticket in doing already has a session on it, and one
    // in blocked, done or archived is not ready to be picked up -- a second window on either
    // is two agents on one ticket, or work on something finished.
    if detail.task.status != Status::Backlog {
        return Err(ApiError(Error::InvalidValue {
            field: "status",
            value: detail.task.status.to_string(),
            valid: "backlog -- only a ticket nobody has started can be started".into(),
        }));
    }
    let Some((first, rest)) = detail.repos.split_first() else {
        return Err(ApiError(Error::InvalidValue {
            field: "repos",
            value: String::new(),
            valid: "at least one repo on the ticket -- the session has to start in a checkout".into(),
        }));
    };
    let add: Vec<String> = rest.iter().map(|r| r.path.clone()).collect();
    let opened = super::launch::open_claude(profile, &first.path, &add, &crate::render::start_prompt(&detail))?;
    store.log_on(project.id, Some(t), Actor::User,
        &format!("started a {} session in {}", profile.label(), first.name))?;
    Ok(Json(json!({ "started_in": first.path, "profile": profile.label(), "opened": opened.label() })))
}

#[derive(Debug, Deserialize, Default)]
pub struct StartBody {
    /// `claude` (the default) or `claude-work` -- see `launch::Profile`.
    pub profile: Option<String>,
}

/// Every launch profile and whether this board allows it, for the ticket panel's buttons and
/// the board's setting.
fn profiles_json(store: &Store, project_id: i64) -> Result<serde_json::Value, Error> {
    let denied = store.denied_profiles(project_id)?;
    Ok(json!(super::launch::Profile::ALL.iter().map(|p| json!({
        "name": p.label(),
        "allowed": !denied.iter().any(|d| d == p.label()),
    })).collect::<Vec<_>>()))
}

#[derive(Debug, Deserialize, Default)]
pub struct ProfileBody {
    pub allowed: Option<bool>,
}

/// Allows or refuses a session profile on one board -- e.g. `claude-work` only on the boards
/// that are work. Locked to the board page like `start_task`: it is half of what decides
/// which setup a session starts in.
pub async fn set_profile(
    State(api): State<Api>, Path((p, name)): Path<(String, String)>, headers: HeaderMap,
    Json(b): Json<ProfileBody>,
) -> ApiResult<Json<serde_json::Value>> {
    same_origin(&headers)?;
    // Validated here, not in core: the store keeps names, the launcher knows which exist.
    let profile = super::launch::Profile::parse(Some(&name))?;
    let Some(allowed) = b.allowed else {
        return Err(ApiError(Error::InvalidValue {
            field: "allowed",
            value: String::new(),
            valid: "true or false".into(),
        }));
    };
    let store = api.store();
    let project = resolve(&store, &p)?;
    store.set_profile_allowed(project.id, profile.label(), allowed, Actor::User)?;
    Ok(Json(json!({ "profiles": profiles_json(&store, project.id)? })))
}

/// The start endpoint launches a program, so unlike every other route it must not be callable
/// by whatever page the browser has open. A cross-site POST carries that site's `Origin`; a
/// DNS-rebinding page carries a `Host` that is not loopback. The JSON body the route requires
/// also forces a CORS preflight on any cross-origin request, which this server never answers.
/// A request with no `Origin` is a local program, which could run `claude` itself anyway.
fn same_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let loopback = name == "127.0.0.1" || name == "localhost";
    let origin_ok = match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o == format!("http://{host}"),
    };
    if loopback && origin_ok {
        Ok(())
    } else {
        Err(ApiError(Error::Forbidden("sessions can only be started from the board page itself".into())))
    }
}

/// Permanent. See `src/core/forget.rs` -- this is the only surface it is reachable from, and
/// deliberately so.
pub async fn forget_task(
    State(api): State<Api>, Path((p, t)): Path<(String, i64)>,
) -> ApiResult<StatusCode> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    store.forget_task(project.id, t)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// All Projects
//
// The person's view across every board. The agent's cross-board view stays a per-board
// summary (`board(project: "all")`): for a reader paying per token, every open task across
// every project is a pile, not a board. A person scanning columns is not paying per token,
// and asked for exactly this.
// ---------------------------------------------------------------------------

type Extras = (Vec<(i64, Status)>, Vec<(i64, Vec<String>)>, Vec<TaskLinks>);

/// Blocker statuses, tags and links for tasks from any number of boards. Every one of those
/// lookups is scoped to a board -- the store is global, and a bare id can name another board's
/// row -- so the rows are grouped by the board they are on and each group asked separately.
fn extras_across(store: &Store, tasks: &[Task]) -> Result<Extras, Error> {
    let mut by_board: std::collections::BTreeMap<i64, Vec<Task>> = std::collections::BTreeMap::new();
    for t in tasks {
        by_board.entry(t.project_id).or_default().push(t.clone());
    }
    let (mut blockers, mut tags, mut links) = (Vec::new(), Vec::new(), Vec::new());
    for (pid, rows) in &by_board {
        blockers.extend(store.blocker_status(*pid, rows)?);
        tags.extend(store.tags_for(*pid, rows)?);
        links.extend(store.links_for(*pid, rows)?);
    }
    Ok((blockers, tags, links))
}

fn tags_json(tags: &[(i64, Vec<String>)]) -> serde_json::Map<String, serde_json::Value> {
    tags.iter().map(|(id, t)| (id.to_string(), json!(t))).collect()
}

/// Every board's tickets in one set of columns. Same shape as `/board`, plus `projects` so a
/// card can name its board, and no workstream -- that is a slice of ONE board.
pub async fn all_board(State(api): State<Api>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    // One read transaction, for the reason `/board` takes one.
    let tx = store.conn.unchecked_transaction()?;
    let counts = store.status_counts_all()?;
    let mut statuses: Vec<Status> = Status::ALL.to_vec();
    statuses.sort_by_key(|s| s.column_rank());

    let mut columns = Vec::new();
    let mut listed: Vec<Task> = Vec::new();
    for s in statuses {
        let page = store.tasks_page_all(&[s], None, PAGE)?;
        listed.extend(page.items.iter().cloned());
        columns.push(json!({
            "status": s.as_str(),
            "total": counts.iter().find(|(k, _)| *k == s).map(|(_, n)| *n).unwrap_or(0),
            "tasks": page.items,
            "next": page.next.map(encode_cursor),
        }));
    }
    let (blockers, tags, links) = extras_across(&store, &listed)?;
    let projects = store.all_projects()?;
    // Which boards track repos, so a card can say "no repo set" by its OWN board's rule.
    let mut with_repos = Vec::new();
    for p in &projects {
        if store.repo_count(p.id)? > 0 {
            with_repos.push(p.id);
        }
    }
    let cursor = store.change_cursor()?;
    tx.commit()?;

    Ok(Json(json!({
        "now": crate::core::now(),
        "cursor": cursor,
        "all": true,
        "projects": projects,
        "boards_with_repos": with_repos,
        "counts": counts.iter().map(|(s, n)| json!({ "status": s.as_str(), "count": n })).collect::<Vec<_>>(),
        "columns": columns,
        "blocker_status": blockers.iter()
            .map(|(id, s)| json!({ "id": id, "status": s.as_str() }))
            .collect::<Vec<_>>(),
        "task_tags": tags_json(&tags),
        "task_links": links_json(&links),
    })))
}

/// A further page of one All Projects column.
pub async fn all_tasks(
    State(api): State<Api>, Query(q): Query<PageParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let page = store.tasks_page_all(
        &parse_status_list(&q.status)?,
        decode_cursor(&q.cursor)?,
        q.limit.unwrap_or(PAGE).min(200),
    )?;
    let (_, tags, links) = extras_across(&store, &page.items)?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "tasks": page.items,
        "task_tags": tags_json(&tags),
        "task_links": links_json(&links),
        "next": page.next.map(encode_cursor),
    })))
}

/// Task links keyed by task id, like `task_tags`. Only tasks with a repo or an external ref appear.
fn links_json(links: &[TaskLinks]) -> serde_json::Map<String, serde_json::Value> {
    links.iter()
        .map(|l| (l.task_id.to_string(), json!({ "repos": l.repos, "external_ref": l.external_ref })))
        .collect()
}

// ---------------------------------------------------------------------------
// Repos
//
// No `If-Match` on these writes, unlike tasks and notes. The guard exists for a form held
// open while an agent overwrites the same row, and a repo has no such row: an agent can now
// register and remove repos (`repo_add`, `repo_remove`), but neither edits a field this form
// owns. A rename is still only from here, and a repo removed underneath an open form fails
// the rename outright rather than losing what was typed into it.
// ---------------------------------------------------------------------------

/// The repos menu: every repo on the board, with how many tickets touch it.
pub async fn repos(State(api): State<Api>, Path(p): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "repos": store.repo_summaries(project.id)?,
    })))
}

#[derive(Debug, Deserialize, Default)]
pub struct RepoBody {
    /// The local checkout, absolute or starting with `~/`. Required when adding.
    pub path: Option<String>,
    /// Defaults to the directory's name when adding.
    pub name: Option<String>,
}

pub async fn create_repo(
    State(api): State<Api>, Path(p): Path<String>, Json(b): Json<RepoBody>,
) -> ApiResult<impl IntoResponse> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let raw = b.path.as_deref().map(str::trim).unwrap_or_default();
    if raw.is_empty() {
        return Err(ApiError(Error::InvalidValue {
            field: "path",
            value: String::new(),
            valid: "the path of a local checkout".into(),
        }));
    }
    let repo = store.add_repo(project.id, &crate::expand_home(raw), b.name.as_deref(), Actor::User)?;
    Ok((StatusCode::CREATED, Json(json!({ "repo": repo }))))
}

/// Renames. The path is not editable: a different path is a different checkout, and it may
/// belong to another board -- so moving a repo is a remove and an add, each of which says
/// what it did and each of which runs its own checks.
pub async fn update_repo(
    State(api): State<Api>, Path((p, r)): Path<(String, i64)>, Json(b): Json<RepoBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    if b.path.is_some() {
        return Err(ApiError(Error::InvalidValue {
            field: "path",
            value: b.path.clone().unwrap_or_default(),
            valid: "unchanged -- remove this repo and add the new path instead".into(),
        }));
    }
    let repo = store.rename_repo(project.id, r, b.name.as_deref().unwrap_or_default(), Actor::User)?;
    Ok(Json(json!({ "repo": repo })))
}

/// Makes this board the repo's home: its folder opens here from now on.
pub async fn repo_home(
    State(api): State<Api>, Path((p, r)): Path<(String, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let repo = store.set_repo_home(project.id, r, Actor::User)?;
    Ok(Json(json!({ "repo": repo })))
}

/// Every repo in the store with the board its folder opens on -- what a board can attach.
pub async fn all_repos(State(api): State<Api>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let names: std::collections::HashMap<i64, String> =
        store.all_projects()?.into_iter().map(|p| (p.id, p.name)).collect();
    let repos: Vec<serde_json::Value> = store.all_repos()?.into_iter()
        .map(|r| json!({ "home_board": names.get(&r.home_project_id), "repo": r }))
        .collect();
    Ok(Json(json!({ "repos": repos })))
}

/// Takes the repo off this board and off this board's tickets. See `Store::remove_repo` for
/// where its folder opens afterwards.
pub async fn delete_repo(
    State(api): State<Api>, Path((p, r)): Path<(String, i64)>,
) -> ApiResult<StatusCode> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    store.remove_repo(project.id, r, Actor::User)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The repo gone from every board, not just this one. Global because repos are: no one board
/// scopes a question about all of them.
pub async fn forget_repo(State(api): State<Api>, Path(r): Path<i64>) -> ApiResult<StatusCode> {
    api.store().forget_repo(r)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn forget_board(State(api): State<Api>, Path(p): Path<String>) -> ApiResult<StatusCode> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    store.forget_board(project.id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

pub async fn notes(
    State(api): State<Api>, Path(p): Path<String>, Query(q): Query<PageParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let page = store.notes_page(project.id, decode_cursor(&q.cursor)?, q.limit.unwrap_or(PAGE).min(200))?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "notes": page.items,
        "next": page.next.map(encode_cursor),
    })))
}

#[derive(Debug, Deserialize, Default)]
pub struct NoteBody {
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
    /// The files this note is about. **The UI must send these.** `note_paths` is what the
    /// PostToolUse hook reads to surface notes contextually, so a note filed with none is one
    /// the agent will never be handed while editing the file it is about -- it exists for the
    /// agent's benefit and never reaches it.
    pub paths: Option<Vec<String>>,
    pub task_id: Option<i64>,
}

pub async fn create_note(
    State(api): State<Api>, Path(p): Path<String>, Json(b): Json<NoteBody>,
) -> ApiResult<impl IntoResponse> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let title = b.title.clone().unwrap_or_default();
    if title.trim().is_empty() {
        return Err(ApiError(Error::InvalidValue {
            field: "title",
            value: title,
            valid: "a non-empty title".into(),
        }));
    }
    let note = store.create_note(
        project.id,
        NoteDraft {
            title,
            body: b.body.clone().unwrap_or_default(),
            tags: b.tags.clone().unwrap_or_default(),
            paths: b.paths.clone().unwrap_or_default(),
            task_id: b.task_id,
        },
        Actor::User,
    )?;
    let version = note.version;
    Ok((StatusCode::CREATED, etag(version), Json(json!({ "note": note }))))
}

pub async fn update_note(
    State(api): State<Api>, Path((p, n)): Path<(String, i64)>, headers: HeaderMap,
    Json(b): Json<NoteBody>,
) -> ApiResult<impl IntoResponse> {
    let expected = if_match(&headers)?;
    let store = api.store();
    let project = resolve(&store, &p)?;
    let note = store.update_note(
        project.id, n,
        NotePatch {
            title: b.title.clone(),
            body: b.body.clone(),
            tags: b.tags.clone(),
            paths: b.paths.clone(),
            actor: Actor::User,
            expected_version: Some(expected),
        },
    )?;
    let version = note.version;
    Ok((etag(version), Json(json!({ "note": note }))))
}

pub async fn forget_note(
    State(api): State<Api>, Path((p, n)): Path<(String, i64)>,
) -> ApiResult<StatusCode> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    store.forget_note(project.id, n)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// History and search
// ---------------------------------------------------------------------------

pub async fn events(
    State(api): State<Api>, Path(p): Path<String>, Query(q): Query<PageParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let page = store.events_page(project.id, decode_cursor(&q.cursor)?, q.limit.unwrap_or(PAGE).min(200))?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "events": page.items,
        "next": page.next.map(encode_cursor),
    })))
}

#[derive(Debug, Deserialize)]
pub struct RecallParams {
    pub q: String,
    /// Omit to search every board -- the cross-project recall no per-repo tool can offer.
    pub project: Option<String>,
    pub limit: Option<usize>,
}

pub async fn recall(
    State(api): State<Api>, Query(p): Query<RecallParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project_id = match &p.project {
        Some(s) if !s.is_empty() && s != "all" => Some(resolve(&store, s)?.id),
        _ => None,
    };
    let result = store.recall(&RecallQuery {
        text: &p.q,
        project_id,
        limit: p.limit.unwrap_or(20).min(100),
    })?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "scope": result.scope,
        "hits": result.hits,
        "omitted": result.omitted,
        "available": result.available,
    })))
}

// ---------------------------------------------------------------------------
// The person's to-do list
//
// No `{p}` on any of these: the list is global, the same list from every board. That is the
// whole shape of the feature and the reason it is not a task -- see `core::todo`.
//
// No `If-Match` either, where every other write here has one. The version guard exists
// because a browser form sits open for minutes while an agent writes the same row from
// another process; this is the one table no agent writes, so the race it guards is not
// reachable. Demanding a version on a checkbox would be ceremony bought with nothing.
//
// Writes require a same-origin request, like `start`: this is a loopback server with no auth,
// so without that check any page in the browser can POST to this list blind.
// ---------------------------------------------------------------------------

/// The list plus its revision. The revision is the cursor's counterpart for this table --
/// the live stream reports it, and a page compares it against what its last fetch returned.
pub async fn todos(State(api): State<Api>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let todos = store.todos()?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "rev": store.todo_rev()?,
        // The number the header badge shows. Computed here rather than in the page so every
        // consumer counts the same thing -- unchecked items, not rows.
        "open": todos.iter().filter(|t| t.done_at.is_none()).count(),
        "todos": todos,
    })))
}

#[derive(Debug, Deserialize)]
pub struct TodoBody {
    pub text: Option<String>,
    pub done: Option<bool>,
}

pub async fn create_todo(
    State(api): State<Api>, headers: HeaderMap, Json(body): Json<TodoBody>,
) -> ApiResult<impl IntoResponse> {
    same_origin(&headers)?;
    let store = api.store();
    let todo = store.add_todo(body.text.as_deref().unwrap_or(""))?;
    Ok((StatusCode::CREATED, Json(json!(todo))))
}

/// Check, uncheck, or fix the wording -- whichever the body names. Both at once is fine; a
/// body naming neither is a no-op rather than an error, because there is nothing to correct.
pub async fn update_todo(
    State(api): State<Api>, Path(id): Path<i64>, headers: HeaderMap, Json(body): Json<TodoBody>,
) -> ApiResult<Json<serde_json::Value>> {
    same_origin(&headers)?;
    let store = api.store();
    let mut todo = store.todo(id)?;
    if let Some(text) = &body.text {
        todo = store.edit_todo(id, text)?;
    }
    if let Some(done) = body.done {
        todo = store.set_todo_done(id, done)?;
    }
    Ok(Json(json!(todo)))
}

pub async fn delete_todo(
    State(api): State<Api>, Path(id): Path<i64>, headers: HeaderMap,
) -> ApiResult<StatusCode> {
    same_origin(&headers)?;
    api.store().remove_todo(id)?;
    Ok(StatusCode::NO_CONTENT)
}
