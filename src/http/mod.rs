//! The HTTP API, and the web UI served from it.
//!
//! The second consumer. `docs/http-api.md` is the design and the reasoning; this implements
//! it. The short version of why it looks different from the MCP adapter:
//!
//! * **JSON, never `render`'s prose.** Raw timestamps plus the server's `now`, so the browser
//!   can tick "2h ago" without refetching.
//! * **The project is always in the path.** MCP captures a `cwd` once at startup; HTTP has no
//!   per-request working directory, so there is no "current project" here.
//! * **Its own paging**, in `core::page`. Widening the agent's caps to fill a web page would
//!   leave `tests/budget.rs` asserting the wrong thing entirely.
//! * **Writes carry a version.** A browser form sits open for minutes while an agent works
//!   the same board; that is the lost update the guard exists for.

pub mod launch;
pub mod routes;
pub mod stream;

use crate::core::{Error, Store};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use std::sync::{Arc, Mutex};

/// Shared state. One connection behind a mutex, like the MCP server -- this is a single-user
/// local tool, and a connection pool would be machinery for a load that does not exist.
///
/// The SSE poller deliberately does **not** use this. See `stream::spawn_poller`.
#[derive(Clone)]
pub struct Api {
    pub store: Arc<Mutex<Store>>,
    pub changes: tokio::sync::broadcast::Sender<stream::Change>,
}

impl Api {
    pub fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The web UI. Embedded in the binary rather than served from disk: "one file to install"
/// is the pitch, and a UI that needs its assets next to it is no longer one file.
const INDEX: &str = include_str!("ui.html");

pub fn router(api: Api) -> Router {
    Router::new()
        .route("/", get(|| async { axum::response::Html(INDEX) }))
        .route("/api/meta", get(routes::meta))
        .route("/api/projects", get(routes::projects).post(routes::create_board))
        .route("/api/all/board", get(routes::all_board))
        .route("/api/all/tasks", get(routes::all_tasks))
        .route("/api/repos", get(routes::all_repos))
        .route("/api/repos/{r}", delete(routes::forget_repo))
        .route("/api/projects/{p}", get(routes::project).delete(routes::forget_board))
        .route("/api/projects/{p}/board", get(routes::board))
        .route("/api/projects/{p}/workstream", put(routes::set_workstream))
        .route("/api/projects/{p}/profiles/{name}", put(routes::set_profile))
        .route("/api/projects/{p}/repos", get(routes::repos).post(routes::create_repo))
        .route("/api/projects/{p}/repos/{r}", patch(routes::update_repo).delete(routes::delete_repo))
        .route("/api/projects/{p}/repos/{r}/home", put(routes::repo_home))
        .route("/api/projects/{p}/tasks/{t}/move", post(routes::move_task))
        .route("/api/projects/{p}/tasks/{t}/start", post(routes::start_task))
        .route("/api/projects/{p}/tasks", get(routes::tasks).post(routes::create_task))
        .route("/api/projects/{p}/tasks/{t}", get(routes::task))
        .route("/api/projects/{p}/tasks/{t}", patch(routes::update_task))
        .route("/api/projects/{p}/tasks/{t}", delete(routes::forget_task))
        .route("/api/projects/{p}/notes", get(routes::notes).post(routes::create_note))
        .route("/api/projects/{p}/notes/{n}", patch(routes::update_note))
        .route("/api/projects/{p}/notes/{n}", delete(routes::forget_note))
        .route("/api/projects/{p}/events", get(routes::events))
        .route("/api/recall", get(routes::recall))
        // Global, not under /api/projects: the person's to-do list is the same list from
        // every board. Nothing on the agent's surface reaches it.
        .route("/api/todos", get(routes::todos).post(routes::create_todo))
        .route("/api/todos/{id}", patch(routes::update_todo).delete(routes::delete_todo))
        .route("/api/stream", get(stream::sse))
        .with_state(api)
}

/// Starts the server. Binds loopback only, and there is deliberately no flag to change that:
/// the store is global, holding every project the user has ever opened and every note about
/// their code, and there is no authentication. A `--host` flag that only ever accepts one
/// value is noise, and the moment it accepts others somebody passes `0.0.0.0`.
pub async fn serve(store: Store, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, _) = tokio::sync::broadcast::channel(64);
    let api = Api { store: Arc::new(Mutex::new(store)), changes: tx.clone() };

    stream::spawn_poller(tx);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("ai-kanban: http://{addr}");
    println!("store: {}", Store::default_path()?.display());
    axum::serve(listener, router(api)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Core errors carry structured context so an adapter can correct the mistake rather than
/// merely report it. That survives into HTTP instead of being flattened to a bare 400: an
/// invalid status comes back with the list of valid ones, and a not-found task comes back
/// with the ids that do exist, so a UI can render a useful message without a second call.
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

/// Handlers open transactions directly, so raw sqlite errors reach `?` here too.
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError(Error::Sqlite(e))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, extra) = match &self.0 {
            Error::TaskNotFound { existing, .. } => (
                StatusCode::NOT_FOUND,
                "not_found",
                serde_json::json!({ "existing": existing }),
            ),
            Error::NoteNotFound { .. } => (StatusCode::NOT_FOUND, "not_found", serde_json::json!({})),
            Error::TodoNotFound { .. } => (StatusCode::NOT_FOUND, "not_found", serde_json::json!({})),
            Error::ProjectNotFound { existing, .. } => (
                StatusCode::NOT_FOUND,
                "not_found",
                serde_json::json!({ "existing": existing }),
            ),
            // 409 with the owner named, so the UI can say which board holds the directory
            // rather than just refusing.
            Error::PathClaimed { board, key, .. } => (
                StatusCode::CONFLICT,
                "claimed",
                serde_json::json!({ "board": board, "key": key }),
            ),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden", serde_json::json!({})),
            Error::AmbiguousProject { candidates, .. } => (
                StatusCode::CONFLICT,
                "ambiguous",
                serde_json::json!({ "candidates": candidates }),
            ),
            Error::InvalidValue { field, valid, .. } => (
                StatusCode::BAD_REQUEST,
                "invalid_value",
                serde_json::json!({ "field": field, "valid": valid.split(", ").collect::<Vec<_>>() }),
            ),
            // The whole point of the guard: the caller is holding a version that is no longer
            // current, and both numbers ride along so the UI can say what happened.
            Error::Conflict { expected, actual, .. } => (
                StatusCode::PRECONDITION_FAILED,
                "conflict",
                serde_json::json!({ "expected": expected, "actual": actual }),
            ),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "error", serde_json::json!({})),
        };
        let mut body = serde_json::json!({ "code": code, "message": self.0.to_string() });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        (status, Json(serde_json::json!({ "error": body }))).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
