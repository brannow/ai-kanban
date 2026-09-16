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
async fn a_board_and_a_repo_can_be_forgotten() {
    let (api, pid) = api();
    let dir = tempfile::tempdir().unwrap();
    let rid = api.store.lock().unwrap().add_repo(pid, dir.path(), None, Actor::User).unwrap().id;
    let del = |path: String| Request::builder().method("DELETE").uri(path).body(Body::empty()).unwrap();

    let (status, _, _) = call(&api, del(format!("/api/repos/{rid}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(api.store.lock().unwrap().all_repos().unwrap().is_empty());

    let (status, _, _) = call(&api, del(format!("/api/projects/{pid}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call(&api, get(&format!("/api/projects/{pid}"))).await;
    assert_ne!(status, StatusCode::OK, "the board is gone");
}

#[tokio::test]
async fn the_board_arrives_in_column_order() {
    // The UI renders columns in the order it receives them, so the server owns the order:
    // backlog on the left, doing in the middle. It is `column_rank`, not the agent's
    // `board_rank` -- sending that one would put "doing" on the far left.
    let (api, pid) = api();
    let (status, body, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;

    assert_eq!(status, StatusCode::OK);
    let order: Vec<&str> = body["columns"].as_array().unwrap().iter()
        .map(|c| c["status"].as_str().unwrap()).collect();
    assert_eq!(order, ["backlog", "blocked", "doing", "testing", "done", "archived"]);
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
    assert_eq!(statuses, ["backlog", "blocked", "doing", "testing", "done", "archived"], "column order");
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


// ---------------------------------------------------------------------------
// Workstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_board_reports_its_scope_and_everything_selectable() {
    let (api, pid) = api();
    {
        let s = api.store.lock().unwrap();
        // Deliberately left with no tasks: a workstream created a moment ago must still be
        // offered by the picker, or the control is quietly lossy. The agent's directory
        // hides empty ones; the human's selector must not.
        s.ensure_workstream(pid, "brand-new").unwrap();
    }
    let (status, body, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["workstream"].is_null(), "nothing is scoped yet");
    let names: Vec<&str> = body["workstreams"].as_array().unwrap().iter()
        .map(|w| w["workstream"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["brand-new"], "an empty workstream must still be selectable");
}

#[tokio::test]
async fn setting_the_workstream_scopes_the_human_board_too() {
    let (api, pid) = api();
    {
        let s = api.store.lock().unwrap();
        let w = s.ensure_workstream(pid, "contact-form").unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
        s.create_task(pid, TaskDraft::new("field validator")).unwrap();
        s.clear_current_workstream(pid).unwrap();
        let o = s.ensure_workstream(pid, "seo-redirects").unwrap();
        s.set_current_workstream(pid, o.id).unwrap();
        s.create_task(pid, TaskDraft::new("canonical tags")).unwrap();
        s.clear_current_workstream(pid).unwrap();
    }

    let (status, body, _) = call(&api, json_req(
        "PUT", &format!("/api/projects/{pid}/workstream"),
        serde_json::json!({ "name": "contact-form" }), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["workstream"]["name"], "contact-form");

    let (_, board, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;
    assert_eq!(board["workstream"]["name"], "contact-form", "the board must state its scope");
    let titles: Vec<&str> = board["columns"].as_array().unwrap().iter()
        .flat_map(|c| c["tasks"].as_array().unwrap())
        .map(|t| t["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"field validator"), "scoped work shows: {titles:?}");
    assert!(!titles.contains(&"canonical tags"), "another workstream's work does not: {titles:?}");

    // And widening back out has to be possible, or a board entered once is stuck.
    let (status, _, _) = call(&api, json_req(
        "PUT", &format!("/api/projects/{pid}/workstream"),
        serde_json::json!({ "name": null }), None)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, board, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;
    assert!(board["workstream"].is_null(), "clearing must widen the board back out");
}

#[tokio::test]
async fn a_person_filing_a_task_decides_its_workstream_rather_than_inheriting_silently() {
    let (api, pid) = api();
    {
        let s = api.store.lock().unwrap();
        let w = s.ensure_workstream(pid, "contact-form").unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
        // Created through the store, the way an agent starts one. The HTTP API can only
        // pick from what already exists.
        s.ensure_workstream(pid, "seo-redirects").unwrap();
    }
    let file = |ws: Value| json_req("POST", &format!("/api/projects/{pid}/tasks"),
        {
            let mut v = serde_json::json!({ "title": "t" });
            if !ws.is_null() { v["workstream"] = ws; }
            v["title"] = Value::String(format!("task {}", v["workstream"]));
            v
        }, None);

    // Omitted: inherits, exactly like the agent's task_add.
    let (status, inherited, _) = call(&api, file(Value::Null)).await;
    assert_eq!(status, StatusCode::CREATED);

    // Present-but-empty: the explicit "general work" escape, the only way a person can file
    // outside the scope while the board is scoped to something.
    let (_, general, _) = call(&api, file(Value::String(String::new()))).await;
    // Named: joins that one, creating it if new.
    let (_, named, _) = call(&api, file(Value::String("seo-redirects".into()))).await;

    let s = api.store.lock().unwrap();
    let cf = s.workstream_by_name(pid, "contact-form").unwrap().unwrap();
    let scoped = s.board(pid, &BoardQuery::board().with_workstream(Some(cf.id))).unwrap();
    let in_scope: Vec<i64> = scoped.tasks.iter().map(|t| t.id).collect();

    assert!(in_scope.contains(&inherited["task"]["id"].as_i64().unwrap()),
        "an omitted workstream must inherit the board's scope");
    assert!(!in_scope.contains(&named["task"]["id"].as_i64().unwrap()),
        "a named workstream must win over the current scope");

    // The general task carries no workstream, so it shows from every scope -- the assertion
    // that distinguishes it is that it is NOT in contact-form's own listing.
    let seo = s.workstream_by_name(pid, "seo-redirects").unwrap().unwrap();
    let seo_board = s.board(pid, &BoardQuery::board().with_workstream(Some(seo.id))).unwrap();
    let seo_ids: Vec<i64> = seo_board.tasks.iter().map(|t| t.id).collect();
    assert!(seo_ids.contains(&general["task"]["id"].as_i64().unwrap()),
        "general work is visible from every scope");
    assert!(seo_ids.contains(&named["task"]["id"].as_i64().unwrap()));
}

#[tokio::test]
async fn a_mis_filed_task_can_be_moved_from_the_panel() {
    let (api, pid) = api();
    let (tid, ver) = {
        let s = api.store.lock().unwrap();
        let w = s.ensure_workstream(pid, "contact-form").unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
        s.ensure_workstream(pid, "typo3-v13-upgrade").unwrap();
        let t = s.create_task(pid, TaskDraft::new("deprecated TCA calls")).unwrap();
        (t.id, t.version)
    };

    let (status, _, _) = call(&api, json_req(
        "PATCH", &format!("/api/projects/{pid}/tasks/{tid}"),
        serde_json::json!({ "workstream": "typo3-v13-upgrade", "log": "belongs to the upgrade" }),
        Some(&ver.to_string()))).await;
    assert_eq!(status, StatusCode::OK);

    // The detail response must carry the workstream, or the panel cannot preselect the
    // control and a person editing a task would silently re-file it.
    let (_, detail, _) = call(&api, get(&format!("/api/projects/{pid}/tasks/{tid}"))).await;
    assert_eq!(detail["workstream"]["name"], "typo3-v13-upgrade");
}

#[tokio::test]
async fn an_unknown_workstream_is_refused_with_the_ones_that_exist() {
    let (api, pid) = api();
    {
        let s = api.store.lock().unwrap();
        s.ensure_workstream(pid, "contact-form").unwrap();
        s.ensure_workstream(pid, "typo3-v13-upgrade").unwrap();
    }
    // The human API lists workstreams, it does not mint them -- an agent starts one when it
    // is told what it is working on, which is the moment the name is actually known. The
    // value here is the typo case: `contact-forms` should say what exists rather than
    // silently becoming a third workstream nobody meant to create.
    let (status, body, _) = call(&api, json_req(
        "PUT", &format!("/api/projects/{pid}/workstream"),
        serde_json::json!({ "name": "contact-forms" }), None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.to_string();
    assert!(msg.contains("contact-form") && msg.contains("typo3-v13-upgrade"),
        "the error must list what does exist: {msg}");

    let (_, board, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;
    assert_eq!(board["workstreams"].as_array().unwrap().len(), 2,
        "and must not have created a third");
}

#[tokio::test]
async fn paging_a_column_stays_inside_the_workstream_the_board_is_scoped_to() {
    // `/tasks` is what a board column pages against, and the column header comes from
    // `status_counts_in`, which is workstream-scoped. An unscoped page two would therefore
    // put more cards in a column than its own header counted -- and would show the person
    // work from a slice of the board they had deliberately narrowed away from.
    let (api, pid) = api();
    {
        let s = api.store.lock().unwrap();
        let w = s.ensure_workstream(pid, "contact-form").unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
        for i in 0..6 {
            s.create_task(pid, TaskDraft::new(format!("scoped {i}"))).unwrap();
        }
        s.clear_current_workstream(pid).unwrap();
        let o = s.ensure_workstream(pid, "seo-redirects").unwrap();
        s.set_current_workstream(pid, o.id).unwrap();
        s.create_task(pid, TaskDraft::new("elsewhere")).unwrap();
        s.clear_current_workstream(pid).unwrap();
        let w = s.ensure_workstream(pid, "contact-form").unwrap();
        s.set_current_workstream(pid, w.id).unwrap();
    }

    // Deliberately paged small, so the row from the other workstream would have room to
    // appear if the endpoint were not scoped.
    let (_, first, _) = call(&api, get(&format!("/api/projects/{pid}/tasks?limit=4"))).await;
    let cursor = first["next"].as_str().expect("more pages").to_string();
    let (status, second, _) =
        call(&api, get(&format!("/api/projects/{pid}/tasks?limit=4&cursor={cursor}"))).await;
    assert_eq!(status, StatusCode::OK, "{second}");

    let titles: Vec<&str> = [&first, &second].iter()
        .flat_map(|b| b["tasks"].as_array().unwrap())
        .map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles.len(), 6, "every scoped task, across both pages: {titles:?}");
    assert!(!titles.contains(&"elsewhere"), "another workstream leaked into a page: {titles:?}");
}

#[tokio::test]
async fn repos_are_registered_linked_and_cleared_through_the_api() {
    let (api, pid) = api();
    let checkout = tempfile::tempdir().unwrap();

    let (status, body, _) = call(&api, json_req(
        "POST", &format!("/api/projects/{pid}/repos"),
        serde_json::json!({ "path": checkout.path(), "name": "EEE Web" }), None,
    )).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["repo"]["name"], "eee-web");
    let rid = body["repo"]["id"].as_i64().unwrap();

    let (status, body, _) = call(&api, json_req(
        "POST", &format!("/api/projects/{pid}/tasks"),
        serde_json::json!({ "title": "invoice rounding", "repos": [rid], "planio": 48213 }), None,
    )).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let tid = body["task"]["id"].as_i64().unwrap();

    // Links ride beside the rows, like tags; every repo rides along for the pickers and menu.
    let (_, board, _) = call(&api, get(&format!("/api/projects/{pid}/board"))).await;
    let links = &board["task_links"][tid.to_string()];
    assert_eq!(links["repos"], serde_json::json!(["eee-web"]), "{board}");
    assert_eq!(links["planio"], 48213);
    assert_eq!(board["repos"][0]["total"], 1);

    // `[]` and `0` clear -- `0` because a JSON null in an optional field reads as "absent".
    let (_, detail, etag) = call(&api, get(&format!("/api/projects/{pid}/tasks/{tid}"))).await;
    assert_eq!(detail["repos"][0]["name"], "eee-web");
    let version = etag.unwrap().trim_matches('"').to_string();
    let (status, body, _) = call(&api, json_req(
        "PATCH", &format!("/api/projects/{pid}/tasks/{tid}"),
        serde_json::json!({ "repos": [], "planio": 0 }), Some(&version),
    )).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, detail, _) = call(&api, get(&format!("/api/projects/{pid}/tasks/{tid}"))).await;
    assert_eq!(detail["repos"], serde_json::json!([]));
    assert!(detail["planio"].is_null());

    // A folder another board already resolves is a 409 naming the owner, so the UI can say
    // which board holds it and point at `merge`.
    let elsewhere = tempfile::tempdir().unwrap();
    let owner = api.store().resolve_project(elsewhere.path()).unwrap().project;
    let (status, body, _) = call(&api, json_req(
        "POST", &format!("/api/projects/{pid}/repos"),
        serde_json::json!({ "path": elsewhere.path() }), None,
    )).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "claimed");
    assert_eq!(body["error"]["key"], owner.key);

    let (status, _, _) = call(&api, json_req(
        "DELETE", &format!("/api/projects/{pid}/repos/{rid}"), serde_json::json!({}), None,
    )).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list, _) = call(&api, get(&format!("/api/projects/{pid}/repos"))).await;
    assert_eq!(list["repos"], serde_json::json!([]));
}

#[tokio::test]
async fn boards_are_created_tickets_move_and_all_projects_shows_every_board() {
    let (api, pid) = api();

    let (status, body, _) = call(&api, json_req("POST", "/api/projects", serde_json::json!({ "name": "BMUKN" }), None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bid = body["project"]["id"].as_i64().unwrap();
    let (status, _, _) = call(&api, json_req("POST", "/api/projects", serde_json::json!({ "name": "bmukn" }), None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a name another board has is refused");

    let tid = api.store().create_task(pid, TaskDraft::new("Rework header slider")).unwrap().id;
    let (_, _, etag) = call(&api, get(&format!("/api/projects/{pid}/tasks/{tid}"))).await;
    let version = etag.unwrap().trim_matches('"').to_string();
    let (status, body, _) = call(&api, json_req(
        "POST", &format!("/api/projects/{pid}/tasks/{tid}/move"),
        serde_json::json!({ "to": bid, "log": "belongs to BMUKN" }), Some(&version),
    )).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project"]["name"], "BMUKN");
    let (status, _, _) = call(&api, get(&format!("/api/projects/{pid}/tasks/{tid}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "it left the old board");

    let (status, all, _) = call(&api, get("/api/all/board")).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all["projects"].as_array().unwrap().len(), 2);
    assert_eq!(all["columns"][0]["status"], "backlog");
    let listed: Vec<i64> = all["columns"].as_array().unwrap().iter()
        .flat_map(|c| c["tasks"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()))
        .collect();
    assert!(listed.contains(&tid), "every board's tickets: {all}");

    // Sharing a repo leaves its home alone; moving the home is its own step.
    let checkout = tempfile::tempdir().unwrap();
    let (_, body, _) = call(&api, json_req("POST", &format!("/api/projects/{pid}/repos"),
        serde_json::json!({ "path": checkout.path() }), None)).await;
    let rid = body["repo"]["id"].as_i64().unwrap();
    let (status, body, _) = call(&api, json_req("POST", &format!("/api/projects/{bid}/repos"),
        serde_json::json!({ "path": checkout.path() }), None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["repo"]["home_project_id"], pid);
    let (status, body, _) = call(&api, Request::builder().method("PUT")
        .uri(format!("/api/projects/{bid}/repos/{rid}/home")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repo"]["home_project_id"], bid);
    let (_, repos, _) = call(&api, get("/api/repos")).await;
    assert_eq!(repos["repos"][0]["home_board"], "BMUKN");
}

fn start_req(path: &str, origin: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri(path)
        .header("content-type", "application/json").header("host", "127.0.0.1:7373");
    if let Some(o) = origin {
        b = b.header("origin", o);
    }
    b.body(Body::from("{}")).unwrap()
}

#[tokio::test]
async fn starting_a_session_is_refused_from_other_sites_and_without_a_repo() {
    // Only the refusals are exercised: the success path opens a real terminal window. They
    // are the half that matters -- an unauthenticated local endpoint that launches programs
    // must not be reachable from whatever page the browser has open.
    let (api, pid) = api();
    let tid = api.store().create_task(pid, TaskDraft::new("Rework header slider")).unwrap().id;
    let path = format!("/api/projects/{pid}/tasks/{tid}/start");

    let (status, body, _) = call(&api, start_req(&path, Some("https://evil.example"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body, _) = call(&api, start_req(&path, Some("http://127.0.0.1:7373"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a ticket with no repo has nowhere to start: {body}");
    assert_eq!(body["error"]["field"], "repos");

    // Only backlog work: a second session on a ticket in doing is two agents on one ticket.
    api.store().update_task(pid, tid, ai_kanban::core::task::TaskPatch {
        status: Some(Status::Doing), ..Default::default()
    }).unwrap();
    let (status, body, _) = call(&api, start_req(&path, Some("http://127.0.0.1:7373"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], "status");

    // Only the two known setups; anything else names them rather than guessing.
    let req = Request::builder().method("POST").uri(&path)
        .header("content-type", "application/json").header("host", "127.0.0.1:7373")
        .body(Body::from(r#"{"profile":"claude-bmu"}"#)).unwrap();
    let (status, body, _) = call(&api, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], "profile");
}
