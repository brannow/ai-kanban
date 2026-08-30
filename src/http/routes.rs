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
/// the day a status is added. Values arrive in display order (`Status::board_rank`,
/// `Priority::rank`) so the client renders columns left to right without knowing the rule.
///
/// Carries no cursor on purpose -- this document is cacheable for the life of the process,
/// and mixing a constantly-changing value into it would make a client refetch the enum table
/// forever.
pub async fn meta(State(api): State<Api>) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let mut statuses: Vec<Status> = Status::ALL.to_vec();
    statuses.sort_by_key(|s| s.board_rank());
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

    let counts = store.status_counts(project.id)?;
    let mut statuses: Vec<Status> = Status::ALL.to_vec();
    statuses.sort_by_key(|s| s.board_rank());

    let mut columns = Vec::new();
    for s in statuses {
        let page = store.tasks_page(project.id, &[s], None, PAGE)?;
        columns.push(json!({
            "status": s.as_str(),
            "total": counts.iter().find(|(k, _)| *k == s).map(|(_, n)| *n).unwrap_or(0),
            "tasks": page.items,
            "next": page.next.map(encode_cursor),
        }));
    }
    let cursor = store.change_cursor()?;
    tx.commit()?;

    Ok(Json(json!({
        "now": crate::core::now(),
        "cursor": cursor,
        "project": project,
        "counts": counts.iter().map(|(s, n)| json!({ "status": s.as_str(), "count": n })).collect::<Vec<_>>(),
        "columns": columns,
    })))
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

pub async fn tasks(
    State(api): State<Api>, Path(p): Path<String>, Query(q): Query<PageParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let store = api.store();
    let project = resolve(&store, &p)?;
    let page = store.tasks_page(
        project.id,
        &parse_status_list(&q.status)?,
        decode_cursor(&q.cursor)?,
        q.limit.unwrap_or(PAGE).min(200),
    )?;
    Ok(Json(json!({
        "now": crate::core::now(),
        "tasks": page.items,
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
        "blocker": detail.blocker,
        "blocking": detail.blocking,
        "notes": detail.notes,
        "events": detail.events,
        "project": detail.project,
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
    /// The *why*. Recorded as the event body -- the field that makes history worth reading.
    pub log: Option<String>,
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
    };
    let task = store.create_task(project.id, draft)?;
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

    let patch = TaskPatch {
        title: b.title.clone(),
        body: b.body.clone(),
        status: enum_field("status", &b.status, Status::parse)?,
        priority: enum_field("priority", &b.priority, Priority::parse)?,
        task_type: enum_field("type", &b.task_type, TaskType::parse)?,
        blocked_by: b.blocked_by,
        log: b.log.clone(),
        actor: Actor::User,
        expected_version: Some(expected),
    };
    let task = store.update_task(project.id, t, patch)?;
    let version = task.version;
    Ok((etag(version), Json(json!({ "task": task }))))
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
        "available": result.available,
    })))
}
