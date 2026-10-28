//! How tasks are named on a shared board (`docs/sync.md`, Identity): `#<creator's id>-<tag>`,
//! the same on every machine, so "I'm on #122-v1" means one task to everyone -- while every
//! board that is not shared renders exactly as it always did.

mod common;
use common::sync::*;

use ai_kanban::core::model::*;
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::{Error, Store};
use ai_kanban::render;

fn patch() -> TaskPatch {
    TaskPatch { actor: Actor::Agent, ..Default::default() }
}

fn board_text(m: &Member) -> String {
    render::board(&m.store.board(m.pid, &BoardQuery::board()).unwrap())
}

fn connected(tag: &str) -> Handles {
    Handles { tag: Some(tag.into()), ..Default::default() }
}

// ---------------------------------------------------------------------------
// Qualifying text
// ---------------------------------------------------------------------------

#[test]
fn a_plain_number_in_a_teammates_text_means_their_task() {
    let mut h = connected("b1");
    h.tasks.insert(12, ("b1".into(), 12));
    assert_eq!(h.text("dup of #12, see #40", Some("v1")), "dup of #12-v1, see #40-v1", "always theirs, known or not");
    assert_eq!(h.text("dup of #12, see #40", None), "dup of #12-b1, see #40", "mine: a known task gets its handle");
    assert_eq!(h.text("dup of #12", Some("b1")), "dup of #12-b1", "my own tag is not foreign");
}

#[test]
fn text_that_is_already_qualified_or_not_a_reference_is_left_alone() {
    let h = connected("b1");
    for s in ["#12-v1 again", "&#12; entity", "abc#12", "issue#7", "#x", "# 12", "no refs", "#12abc"] {
        assert_eq!(h.text(s, Some("v1")), s, "{s:?}");
    }
    assert_eq!(h.text("#5", Some("v1")), "#5-v1", "at the very start");
    assert_eq!(h.text("(#5)", Some("v1")), "(#5-v1)");
    assert_eq!(h.text("#5\n#6", Some("v1")), "#5-v1\n#6-v1");
}

#[test]
fn a_store_with_nothing_shared_never_rewrites_text() {
    let h = Handles::default();
    assert_eq!(h.text("see #12", Some("v1")), "see #12");
    assert_eq!(h.task(12), "#12");
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn a_shared_board_names_every_task_by_its_creator_and_a_private_one_by_its_plain_id() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let b = benny.store.create_task(benny.pid, TaskDraft::new("benny files this")).unwrap();
    let v = volker.store.create_task(volker.pid, TaskDraft::new("volker files this")).unwrap();
    let private = benny.store.create_board("mine").unwrap().id;
    let p = benny.store.create_task(private, TaskDraft::new("private")).unwrap();
    converge(&relay, &[&benny, &volker]);

    for (who, m) in [("benny", &benny), ("volker", &volker)] {
        let text = board_text(m);
        assert!(text.contains(&format!("#{}-b1", b.id)), "{who}: {text}");
        assert!(text.contains(&format!("#{}-v1", v.id)), "{who}: {text}");
    }
    let private_text = render::board(&benny.store.board(private, &BoardQuery::board()).unwrap());
    assert!(private_text.contains(&format!("#{:<4} private", p.id)), "{private_text}");
    assert!(!private_text.contains("-b1"), "{private_text}");
}

/// Connecting one board must not change a single byte of another board's output.
#[test]
fn connecting_one_board_leaves_every_other_board_byte_identical() {
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let private = s.create_board("private").unwrap().id;
    let shared = s.create_board("shared").unwrap().id;
    let a = s.create_task(private, TaskDraft::new("a")).unwrap();
    s.create_task(private, TaskDraft { blocked_by: Some(a.id), ..TaskDraft::new("b") }).unwrap();
    s.log(private, Actor::Agent, &format!("see #{}", a.id)).unwrap();
    let before = render::board(&s.board(private, &BoardQuery::board()).unwrap());
    let detail_before = render::task_detail(&s.task_detail(private, a.id).unwrap());
    s.connect_board(shared, "room").unwrap();
    s.create_task(shared, TaskDraft::new("shared work")).unwrap();
    s.flush().unwrap();
    assert_eq!(render::board(&s.board(private, &BoardQuery::board()).unwrap()), before);
    assert_eq!(render::task_detail(&s.task_detail(private, a.id).unwrap()), detail_before);
}

#[test]
fn a_teammates_log_reads_with_their_numbers_qualified() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("auth")).unwrap();
    let other = benny.store.create_task(benny.pid, TaskDraft::new("other")).unwrap();
    benny.store.log_on(benny.pid, Some(t.id), Actor::Agent, &format!("same root cause as #{}", other.id)).unwrap();
    converge(&relay, &[&benny, &volker]);

    let vt = task_by_title(&volker, "auth");
    let shown = render::task_detail(&volker.store.task_detail(volker.pid, vt.id).unwrap());
    assert!(shown.contains(&format!("same root cause as #{}-b1", other.id)), "{shown}");
    assert!(shown.contains(&format!("#{}-b1 auth", t.id)), "the header names the task by its handle: {shown}");
    // And Benny, reading his own text, sees the same spelling.
    let mine = render::task_detail(&benny.store.task_detail(benny.pid, t.id).unwrap());
    assert!(mine.contains(&format!("same root cause as #{}-b1", other.id)), "{mine}");
}

#[test]
fn generated_history_names_a_teammates_task_by_its_handle() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let v = volker.store.create_task(volker.pid, TaskDraft::new("their blocker")).unwrap();
    let b = benny.store.create_task(benny.pid, TaskDraft::new("mine")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let theirs = task_by_title(&benny, "their blocker");
    benny.store.update_task(benny.pid, b.id, TaskPatch { blocked_by: Some(Some(theirs.id)), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);

    // The stored text, as it travels: the handle, never Benny's hidden local number.
    let stored: String = benny.store.conn.query_row(
        "SELECT body FROM events WHERE task_id = ?1 AND kind = 'updated' ORDER BY id DESC LIMIT 1", [b.id], |r| r.get(0)).unwrap();
    assert_eq!(stored, format!("blocked by #{}-v1", v.id));
    let vb = task_by_title(&volker, "mine");
    let shown = render::task_detail(&volker.store.task_detail(volker.pid, vb.id).unwrap());
    assert!(shown.contains(&format!("blocked by #{}-v1", v.id)), "{shown}");
    let line = board_text(&volker);
    assert!(line.contains(&format!("blocked by #{}-v1", v.id)), "{line}");
}

#[test]
fn not_found_lists_the_tasks_by_the_spelling_that_resolves() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("real")).unwrap();
    benny.store.flush().unwrap();
    let err = benny.store.task(benny.pid, 99_999).unwrap_err();
    let text = render::error(&err);
    assert!(text.contains(&format!("#{}-b1 real", t.id)), "{text}");
}

// ---------------------------------------------------------------------------
// What an agent can pass
// ---------------------------------------------------------------------------

#[test]
fn a_task_can_be_named_by_handle_or_by_a_number_created_here() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let mine = benny.store.create_task(benny.pid, TaskDraft::new("mine")).unwrap();
    let theirs_there = volker.store.create_task(volker.pid, TaskDraft::new("theirs")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let theirs_here = task_by_title(&benny, "theirs");
    let s = &benny.store;

    assert_eq!(s.resolve_task_arg(benny.pid, &mine.id.to_string()).unwrap(), mine.id);
    assert_eq!(s.resolve_task_arg(benny.pid, &format!("#{}", mine.id)).unwrap(), mine.id);
    assert_eq!(s.resolve_task_arg(benny.pid, &format!("{}-b1", mine.id)).unwrap(), mine.id);
    assert_eq!(s.resolve_task_arg(benny.pid, &format!(" #{}-v1 ", theirs_there.id)).unwrap(), theirs_here.id);

    // The hidden local number of a teammate's task is refused, with the handle to use.
    let err = s.resolve_task_arg(benny.pid, &theirs_here.id.to_string()).unwrap_err();
    let msg = render::error(&err);
    assert!(msg.contains(&format!("#{}-v1", theirs_there.id)), "{msg}");

    assert!(matches!(s.resolve_task_arg(benny.pid, "999-v1"), Err(Error::InvalidValue { .. })));
    assert!(matches!(s.resolve_task_arg(benny.pid, "banana"), Err(Error::InvalidValue { .. })));
    assert!(matches!(s.resolve_task_arg(benny.pid, "12-V1"), Err(Error::InvalidValue { .. })));
}

#[test]
fn a_handle_from_another_board_does_not_resolve_here() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("on the shared board")).unwrap();
    benny.store.flush().unwrap();
    let other = benny.store.create_board("other").unwrap().id;
    assert!(benny.store.resolve_task_arg(other, &format!("{}-b1", t.id)).is_err(), "task lookups take a board");
}

#[test]
fn a_store_with_nothing_shared_takes_numbers_as_before_and_refuses_handles() {
    let s = Store::open_in_memory().unwrap();
    let pid = s.create_board("b").unwrap().id;
    let t = s.create_task(pid, TaskDraft::new("t")).unwrap();
    assert_eq!(s.resolve_task_arg(pid, &t.id.to_string()).unwrap(), t.id);
    assert!(s.resolve_task_arg(pid, "12-v1").is_err());
}

#[test]
fn the_task_param_is_a_string_in_the_schema_and_still_takes_an_integer() {
    use ai_kanban::mcp::server::{TaskShowParams, TaskUpdateParams};
    let p: TaskUpdateParams = serde_json::from_value(serde_json::json!({ "task": 12 })).unwrap();
    assert_eq!(p.task.0, "12");
    let p: TaskUpdateParams = serde_json::from_value(serde_json::json!({ "task": "122-v1", "blocked_by": 0 })).unwrap();
    assert_eq!((p.task.0.as_str(), p.blocked_by.unwrap().0.as_str()), ("122-v1", "0"));
    let p: TaskShowParams = serde_json::from_value(serde_json::json!({ "task": "#7-b1" })).unwrap();
    assert_eq!(p.task.0, "#7-b1");

    let tools = ai_kanban::mcp::schema::portable_tools(ai_kanban::mcp::server::AiKanban::tool_router().list_all());
    for (tool, field) in [("task_update", "task"), ("task_show", "task"), ("task_add", "blocked_by"), ("log", "task"), ("note_add", "task")] {
        let t = tools.iter().find(|t| t.name == tool).unwrap();
        let prop = &t.input_schema["properties"][field];
        let s = prop.to_string();
        assert!(s.contains("\"string\""), "{tool}.{field}: {s}");
        assert!(!s.contains("$ref"), "{tool}.{field} is inline: {s}");
    }
}

#[test]
fn a_note_number_in_generated_history_is_never_taken_for_a_task() {
    use ai_kanban::core::note::{NoteDraft, NotePatch};
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("t")).unwrap();
    let n = benny.store.create_note(benny.pid, NoteDraft { task_id: Some(t.id), ..NoteDraft::new("fact") }, Actor::Agent).unwrap();
    // Task and note ids are separate sequences, so in a fresh store they collide -- the case
    // that went wrong: "note #1" read as task #1.
    assert_eq!(n.id, t.id, "precondition: the note's id is also a synced task's id");
    benny.store.update_note(benny.pid, n.id, NotePatch { body: Some("better".into()), actor: Actor::Agent, ..Default::default() }).unwrap();
    converge(&relay, &[&benny, &volker]);

    for m in [&benny, &volker] {
        let task = task_by_title(m, "t");
        let shown = render::task_detail(&m.store.task_detail(m.pid, task.id).unwrap());
        assert!(shown.contains(&format!("note #{} \"fact\"", n.id)), "{}: {shown}", m.p.tag);
        assert!(!shown.contains(&format!("note #{}-", n.id)), "{}: a note number became a task handle: {shown}", m.p.tag);
    }
    let h = Handles { tag: Some("b1".into()), ..Default::default() };
    assert_eq!(h.text("note #3 \"x\" -- was: see #4", Some("v1")), "note #3 \"x\" -- was: see #4-v1");
    assert_eq!(h.text("Note #3", Some("v1")), "Note #3");
}

// ---------------------------------------------------------------------------
// The shared board's header
// ---------------------------------------------------------------------------

fn header(s: &Store, pid: i64) -> String {
    render::board(&s.board(pid, &BoardQuery::board()).unwrap()).lines().take(2).collect::<Vec<_>>().join("\n")
}

#[test]
fn a_shared_board_names_its_room_its_team_and_anything_wrong_with_its_sync() {
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("sbb").unwrap().id;
    s.connect_board(pid, "sbahn").unwrap();
    assert!(header(&s, pid).starts_with("Board: sbb @sbahn  ("), "{}", header(&s, pid));
    assert!(header(&s, pid).contains("sync: not synced yet"), "{}", header(&s, pid));

    let team = vec![("b1".to_string(), "benny".to_string()), ("v1".to_string(), "volker".to_string())];
    s.flush().unwrap();
    s.outbox_sent(pid, i64::MAX).unwrap();
    s.sync_succeeded(pid, &team).unwrap();
    assert!(header(&s, pid).ends_with("team: b1 benny (you), v1 volker"), "nothing wrong, nothing said: {}", header(&s, pid));

    s.create_task(pid, TaskDraft::new("written offline")).unwrap();
    s.flush().unwrap();
    use ai_kanban::remote::RemoteError;
    s.sync_failed(pid, &RemoteError::Offline("connection refused".into()).recorded()).unwrap();
    let h = header(&s, pid);
    assert!(h.contains("team: b1 benny (you), v1 volker  (offline, ") && h.ends_with(" changes unsent)"), "{h}");
    // Anything but unreachable needs someone to act, and says what.
    s.sync_failed(pid, &RemoteError::Unauthorized.recorded()).unwrap();
    assert!(header(&s, pid).contains("(sync failing: key rejected, "), "{}", header(&s, pid));
    s.sync_failed(pid, &RemoteError::NotFound("no room sbahn".into()).recorded()).unwrap();
    assert!(header(&s, pid).contains("sync failing: room not found"), "{}", header(&s, pid));

    s.set_paused(Some("the relay speaks protocol 2-3, this ai-kanban speaks 1")).unwrap();
    assert!(header(&s, pid).contains("sync paused: the relay speaks protocol 2-3"), "{}", header(&s, pid));
    s.set_paused(None).unwrap();

    s.sync_succeeded(pid, &team).unwrap();
    s.conn.execute("UPDATE sync_room_status SET last_ok_at = last_ok_at - 3 * 3600", []).unwrap();
    assert!(header(&s, pid).contains("last sync 3h"), "a stale copy says so: {}", header(&s, pid));
}

#[test]
fn a_board_that_is_not_shared_has_the_same_header_as_ever() {
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let private = s.create_board("private").unwrap().id;
    let shared = s.create_board("shared").unwrap().id;
    s.connect_board(shared, "room").unwrap();
    assert_eq!(header(&s, private).lines().next().unwrap(), "Board: private  (0 open)");
    assert!(!header(&s, private).contains("team") && !header(&s, private).contains("sync"));
}

// ---------------------------------------------------------------------------
// The web page (#1065): the same naming the agent sees
// ---------------------------------------------------------------------------

async fn get_json(store: Store, uri: &str) -> serde_json::Value {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let (tx, _) = tokio::sync::broadcast::channel(4);
    let api = ai_kanban::http::Api { store: std::sync::Arc::new(std::sync::Mutex::new(store)), changes: tx };
    let res = ai_kanban::http::router(api).oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    assert!(res.status().is_success(), "{uri}: {}", res.status());
    serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap()).unwrap()
}

#[tokio::test]
async fn the_web_page_gets_handles_and_qualified_text_for_a_shared_board() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let mine = volker.store.create_task(volker.pid, TaskDraft::new("volker's own")).unwrap();
    let theirs = volker.store.create_task(volker.pid, TaskDraft::new("needs zebracorn")).unwrap();
    volker.store.log_on(volker.pid, Some(theirs.id), Actor::Agent, &format!("same cause as #{}", mine.id)).unwrap();
    converge(&relay, &[&benny, &volker]);
    let here = task_by_title(&benny, "needs zebracorn");
    let pid = benny.pid;

    // One router over Benny's store for all three reads: the store moves into it.
    let task = {
        let (tx, _) = tokio::sync::broadcast::channel(4);
        let api = ai_kanban::http::Api { store: std::sync::Arc::new(std::sync::Mutex::new(benny.store)), changes: tx };
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let mut out = vec![];
        for uri in [format!("/api/projects/{pid}/board"), format!("/api/projects/{pid}/tasks/{}", here.id), "/api/recall?q=zebracorn".to_string()] {
            let res = ai_kanban::http::router(api.clone()).oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap()).await.unwrap();
            assert!(res.status().is_success(), "{uri}");
            out.push(serde_json::from_slice::<serde_json::Value>(&axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap()).unwrap());
        }
        out
    };
    let (board, detail, recall) = (&task[0], &task[1], &task[2]);
    assert_eq!(board["handles"][here.id.to_string()], format!("#{}-v1", theirs.id), "{}", board["handles"]);
    let log = detail["events"].as_array().unwrap().iter().find(|e| e["kind"] == "log").unwrap();
    assert_eq!(log["body"], format!("same cause as #{}", mine.id), "the stored text is unchanged");
    let key = format!("event:{}", log["id"]);
    assert_eq!(detail["texts"][&key], format!("same cause as #{}-v1", mine.id), "{}", detail["texts"]);
    assert_eq!(recall["handles"][here.id.to_string()], format!("#{}-v1", theirs.id));
}

#[tokio::test]
async fn a_store_that_shares_nothing_answers_the_web_page_exactly_as_before() {
    let s = Store::open_in_memory().unwrap();
    let pid = s.create_board("b").unwrap().id;
    let t = s.create_task(pid, TaskDraft::new("t")).unwrap();
    s.log_on(pid, Some(t.id), Actor::Agent, &format!("see #{}", t.id)).unwrap();
    let board = get_json(s, &format!("/api/projects/{pid}/board")).await;
    assert!(board.get("handles").is_none() && board.get("texts").is_none(), "{board}");
}
