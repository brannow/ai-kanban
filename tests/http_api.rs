//! The HTTP API, exercised through the assembled router.
//!
//! Through `oneshot` on the `Router` rather than by calling handler functions directly,
//! because the things most likely to be wrong are the things a handler-level test cannot
//! see: which path a route is mounted at, whether a query parameter deserializes, what status
//! code comes back, and whether `If-Match` is actually enforced. A handler test would pass
//! happily while a route was mounted at the wrong URL.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;
use ai_kanban::http::{router, Api};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn api() -> (Api, i64) {
    let store = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = store.resolve_project(dir.path()).unwrap().project.id;
    let (tx, _) = tokio::sync::broadcast::channel(16);
    (Api { store: Arc::new(Mutex::new(store)), changes: tx }, pid)
}

async fn call(api: &Api, req: Request<Body>) -> (StatusCode, Value, Option<String>) {
    let res = router(api.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let etag = res.headers().get("etag").and_then(|v| v.to_str().ok()).map(String::from);
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body, etag)
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

fn json_req(method: &str, path: &str, body: Value, if_match: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path).header("content-type", "application/json");
    if let Some(v) = if_match {
        b = b.header("if-match", format!("\"{v}\""));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_board_arrives_in_column_order() {
    // The UI renders columns in the order it receives them. `Status::ALL` is declaration
    // order (backlog first); `board_rank` is display order. Sending the wrong one puts the
    // backlog where "doing" belongs.
    let (api, pid) = api();
    let (status, body, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;

    assert_eq!(status, StatusCode::OK);
    let order: Vec<&str> = body["columns"].as_array().unwrap().iter()
        .map(|c| c["status"].as_str().unwrap()).collect();
    assert_eq!(order, ["doing", "blocked", "backlog", "done", "archived"]);
    assert!(body["cursor"].is_number(), "a board read carries the cursor it was taken at");
    assert!(body["now"].is_number(), "raw timestamps plus now, so the browser can tick ages");
}

#[tokio::test]
async fn a_write_without_if_match_is_refused() {
    // The guard's whole point. A client that forgets the header must not silently get
    // last-write-wins -- that is the lost update the version column exists to prevent.
    let (api, pid) = api();
    let t = api.store().create_task(pid, TaskDraft::new("a task")).unwrap();

    let (status, body, _) = call(&api, json_req(
        "PATCH", &format!("/api/projects/{pid}/tasks/{}", t.id),
        serde_json::json!({ "status": "doing" }), None,
    )).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["field"], "If-Match");
}

#[tokio::test]
async fn a_stale_write_is_a_conflict_carrying_both_versions() {
    let (api, pid) = api();
    let t = api.store().create_task(pid, TaskDraft::new("a task")).unwrap();

    let ok = json_req("PATCH", &format!("/api/projects/{pid}/tasks/{}", t.id),
        serde_json::json!({ "status": "doing" }), Some("1"));
    let (status, _, etag) = call(&api, ok).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"2\""), "the ETag is the new version");

    // Same version again: this is the browser form that sat open while an agent moved it.
    let stale = json_req("PATCH", &format!("/api/projects/{pid}/tasks/{}", t.id),
        serde_json::json!({ "status": "backlog" }), Some("1"));
    let (status, body, _) = call(&api, stale).await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(body["error"]["expected"], 1);
    assert_eq!(body["error"]["actual"], 2);
}

#[tokio::test]
async fn writes_are_attributed_to_the_user_whatever_the_client_says() {
    // `origin` and `actor` are the instrumentation for "does the agent file work unprompted",
    // and that number is already only a floor. If a client could set it, the floor could be
    // inflated and the one metric the project judges itself by would be worthless.
    let (api, pid) = api();
    let (status, body, _) = call(&api, json_req(
        "POST", &format!("/api/projects/{pid}/tasks"),
        serde_json::json!({ "title": "filed by a human", "origin": "agent", "actor": "agent" }),
        None,
    )).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["task"]["origin"], "user", "the client must not be able to claim otherwise");
}

#[tokio::test]
async fn an_invalid_enum_comes_back_with_the_valid_values() {
    // Core errors carry structured context so an adapter can correct the mistake rather than
    // just report it. That has to survive into HTTP, or the UI has to hardcode the list.
    let (api, pid) = api();
    let t = api.store().create_task(pid, TaskDraft::new("a task")).unwrap();

    let (status, body, _) = call(&api, json_req(
        "PATCH", &format!("/api/projects/{pid}/tasks/{}", t.id),
        serde_json::json!({ "status": "wibble" }), Some("1"),
    )).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let valid: Vec<&str> = body["error"]["valid"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap()).collect();
    assert!(valid.contains(&"doing"), "got {valid:?}");
}

#[tokio::test]
async fn meta_carries_the_enums_so_the_ui_never_hardcodes_them() {
    let (api, _) = api();
    let (status, body, _) = call(&api, get("/api/meta")).await;

    assert_eq!(status, StatusCode::OK);
    let statuses: Vec<&str> = body["statuses"].as_array().unwrap()
        .iter().map(|s| s["value"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["doing", "blocked", "backlog", "done", "archived"], "display order");
    assert!(body["schema_version"].as_i64().unwrap() >= 1);
    assert!(body["cursor"].is_null(), "meta is cacheable and must not carry a moving value");
}

#[tokio::test]
async fn an_unknown_project_says_what_does_exist() {
    let (api, _) = api();
    let (status, body, _) = call(&api, get("/api/projects/nope/board")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"]["existing"].is_array(), "errors correct the mistake, not just report it");
}

#[tokio::test]
async fn paging_is_keyset_and_never_repeats_a_row() {
    // The reason for keyset over OFFSET: an agent writes while a person scrolls. Under
    // OFFSET a row inserted above the window shifts everything down, so page two repeats a
    // row page one already showed. Here a write between pages must not do that.
    let (api, pid) = api();
    for i in 0..10 {
        api.store().create_task(pid, TaskDraft::new(format!("task {i}"))).unwrap();
    }

    let (_, first, _) = call(&api, get(&format!("/api/projects/{pid}/tasks?limit=4"))).await;
    let cursor = first["next"].as_str().expect("more pages").to_string();
    let page1: Vec<i64> = first["tasks"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect();
    assert_eq!(page1.len(), 4);

    // Somebody files a task while the reader is between pages.
    api.store().create_task(pid, TaskDraft::new("inserted mid-scroll")).unwrap();

    let (status, second, _) = call(&api, get(&format!("/api/projects/{pid}/tasks?limit=4&cursor={cursor}"))).await;
    assert_eq!(status, StatusCode::OK, "the cursor path must actually execute: {second}");
    let page2: Vec<i64> = second["tasks"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect();

    assert!(page2.iter().all(|id| !page1.contains(id)), "page 2 {page2:?} overlaps page 1 {page1:?}");
}

#[tokio::test]
async fn a_garbage_cursor_is_a_bad_request_not_a_panic() {
    let (api, pid) = api();
    let (status, _, _) = call(&api, get(&format!("/api/projects/{pid}/tasks?cursor=not-a-cursor"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_forgets_the_content_not_just_the_row() {
    // The only surface `forget` is reachable from. Deleting the row is the easy half; the
    // content that could not be removed lived in FTS-indexed event bodies.
    let (api, pid) = api();
    let n = api.store().create_note(
        pid,
        NoteDraft { body: "AKIAsecret123".into(), ..NoteDraft::new("a leak") },
        Actor::Agent,
    ).unwrap();

    let (found, _, _) = call(&api, get("/api/recall?q=AKIAsecret123")).await;
    assert_eq!(found, StatusCode::OK);

    let req = Request::builder().method("DELETE")
        .uri(format!("/api/projects/{pid}/notes/{}", n.id))
        .body(Body::empty()).unwrap();
    let (status, _, _) = call(&api, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body, _) = call(&api, get("/api/recall?q=AKIAsecret123")).await;
    assert!(body["hits"].as_array().unwrap().is_empty(), "the note body must be unfindable");
}

#[tokio::test]
async fn the_ui_is_served_from_the_binary() {
    // Embedded rather than read from disk: "one file to install" stops being true the moment
    // the UI needs its assets next to it.
    let (api, _) = api();
    let res = router(api).oneshot(get("/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024).await.unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("<title>ai-kanban</title>"));
    assert!(html.contains("EventSource"), "the page must actually subscribe to live updates");
}

