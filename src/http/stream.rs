//! Live updates.
//!
//! # Why SSE and not WebSocket
//!
//! The flow is one-directional: the server says "the board changed". The browser's writes are
//! ordinary requests that want status codes, error bodies and `If-Match`; carrying those over
//! a socket means rebuilding request/response correlation and retry by hand, to gain a
//! direction that is never used.
//!
//! The deciding argument is resume. `EventSource` reconnects on its own and resends the last
//! id it saw as `Last-Event-ID` -- and this schema already has the monotonic counter that
//! maps onto: `events.id`. Resume after a dropped connection is therefore free, where
//! WebSocket would need manual reconnect, manual heartbeat, and a resume protocol invented
//! for the occasion.
//!
//! # Why it polls
//!
//! The server does not own the writes. Agent MCP sessions are separate processes on the same
//! SQLite file, so nothing tells this process when they write. `update_hook` only fires for
//! changes made on the same connection, and SQLite has no `LISTEN`/`NOTIFY`. So: poll
//! `MAX(events.id)`, which is one indexed row.
//!
//! Watching the `-wal` file with a filesystem notifier would cut idle wakeups and is a
//! reasonable later optimisation. It is deliberately not here: it adds a dependency and a
//! per-platform surface to save some timer ticks on a single-user local tool.
//!
//! # What is pushed
//!
//! A cursor and a list of changed projects -- never rows. The client refetches what it is
//! showing. Pushing changed rows would mean maintaining a second representation of every
//! mutation alongside the REST one, for a board small enough to refetch in one request.

use super::Api;
use crate::core::Store;
use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;

/// How often the store is checked. Fast enough that a board feels live, slow enough that an
/// idle server is doing nothing measurable.
const POLL: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
pub struct Change {
    pub cursor: i64,
    pub projects: Vec<i64>,
    /// The to-do list's revision. It rides on the same change frame rather than getting a
    /// stream of its own: a second SSE endpoint would mean a second connection and a second
    /// reconnect protocol for one integer.
    pub todos: i64,
    /// Whether *this* frame carries a to-do change. A client watching one board filters on
    /// `projects`, and the to-do list belongs to no project -- without this flag it would
    /// filter out the only frame that was about its list.
    pub todos_changed: bool,
}

/// Watches the store and fans changes out to every connected client.
///
/// **One poller for the whole server, with its own connection**, and both halves matter.
///
/// One poller, because the obvious shape -- a task per connection -- turns ten open tabs into
/// ten times the query load for no benefit.
///
/// Its own read-only connection rather than the shared `Arc<Mutex<Store>>`, because taking
/// that mutex four times a second would contend with every request handler, and worse: a read
/// transaction held here is a transaction an agent's `task_add` in another process waits on
/// through `busy_timeout`. A viewer must never slow down the thing being viewed.
pub fn spawn_poller(tx: broadcast::Sender<Change>) {
    tokio::spawn(async move {
        let mut last: Option<i64> = None;
        let mut last_todos: Option<i64> = None;
        loop {
            tokio::time::sleep(POLL).await;
            // Reopened per tick, deliberately: the store may not exist yet, may be replaced,
            // and every failure here must be survivable rather than ending the task. Opening
            // a SQLite connection is cheap; a dead poller is not recoverable without a restart.
            let Ok(Some(store)) = Store::open_existing() else { continue };
            let Ok(cursor) = store.change_cursor() else { continue };
            // The to-do list writes no events, so it is invisible to `change_cursor` by
            // design -- see `migrations/011_todos.sql`. Its own counter is polled beside it.
            let todos = store.todo_rev().unwrap_or(0);

            let (Some(previous), Some(previous_todos)) = (last, last_todos) else {
                last = Some(cursor);
                last_todos = Some(todos);
                continue;
            };

            let todos_changed = todos != previous_todos;
            last_todos = Some(todos);

            if cursor <= previous {
                // A cursor that went *down* means a different store (the file was replaced, or
                // AI_KANBAN_DB was repointed). Adopt it rather than waiting for it to climb
                // back past a high-water mark it may never reach.
                if cursor < previous {
                    last = Some(cursor);
                }
                // A to-do change is a real change even when no task moved, so it is sent on
                // its own rather than waiting for the board to do something.
                if todos_changed {
                    let _ = tx.send(Change { cursor, projects: vec![], todos, todos_changed });
                }
                continue;
            }

            let projects = store.projects_changed_since(previous).unwrap_or_default();
            last = Some(cursor);
            // Err means nobody is listening, which is the normal state of an open server.
            let _ = tx.send(Change { cursor, projects, todos, todos_changed });
        }
    });
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// Omit to watch every board, which is what a multi-project overview wants.
    pub project: Option<i64>,
}

pub async fn sse(
    State(api): State<Api>, Query(p): Query<StreamParams>, headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let mut rx = api.changes.subscribe();
    let watching = p.project;

    // Where the client left off, if the browser is reconnecting on its own.
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok());

    let (current, todos) = {
        let store = api.store();
        (store.change_cursor().unwrap_or(0), store.todo_rev().unwrap_or(0))
    };

    let stream = async_stream::stream! {
        // The first event tells the client where the store actually is, so it can compare
        // against the cursor its initial fetch returned and refetch only if it is behind.
        //
        // `reset` when the resume point is ahead of the store: the id names an event that no
        // longer exists, because the store was replaced or renumbered by an import. Replaying
        // from zero would send the entire history; treating it as an error would strand the
        // client. Telling it to start over is the only honest answer.
        let reset = resume.is_some_and(|r| r > current);
        yield Ok::<_, Infallible>(
            SseEvent::default()
                .id(current.to_string())
                .event(if reset { "reset" } else { "change" })
                .json_data(serde_json::json!({ "cursor": current, "todos": todos, "reset": reset }))
                .unwrap_or_else(|_| SseEvent::default().comment("bad payload")),
        );

        loop {
            match rx.recv().await {
                Ok(change) => {
                    // Only wake a client whose board actually changed. A watcher of every
                    // board wakes on everything.
                    if let Some(pid) = watching {
                        // The to-do list belongs to no board, so a frame carrying a to-do
                        // change is for every client whatever it is watching.
                        if !change.todos_changed && !change.projects.contains(&pid) {
                            continue;
                        }
                    }
                    yield Ok(SseEvent::default()
                        .id(change.cursor.to_string())
                        .event("change")
                        .json_data(serde_json::json!({
                            "cursor": change.cursor,
                            "projects": change.projects,
                            "todos": change.todos,
                            "reset": false,
                        }))
                        .unwrap_or_else(|_| SseEvent::default().comment("bad payload")));
                }
                // Lagged: this client fell far enough behind that messages were dropped. It
                // cannot know what it missed, so it is told to refetch rather than shown a
                // cursor that would imply it is up to date.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(SseEvent::default()
                        .event("reset")
                        .json_data(serde_json::json!({ "reset": true }))
                        .unwrap_or_else(|_| SseEvent::default().comment("bad payload")));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    // Comment frames on an idle stream, so an intermediary does not close it as dead.
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}
