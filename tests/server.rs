//! The relay, through its assembled router: auth, rooms, the change log, forget, and the
//! guards that keep a client from writing under someone else's name or reissuing ids after a
//! restore. `docs/sync.md` is the design these assert.

use ai_kanban::server::{router, Relay, ServerDb};
use ai_kanban::sync::protocol::{Change, Clock, Ref};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    relay: Relay,
    benny: String,
    volker: String,
}

fn fixture() -> Fixture {
    let db = ServerDb::open_in_memory().unwrap();
    db.add_user("benny").unwrap();
    db.add_user("volker").unwrap();
    let benny = db.add_key("benny", "b1", "laptop").unwrap();
    let volker = db.add_key("volker", "v1", "laptop").unwrap();
    Fixture { relay: Relay::new(db), benny, volker }
}

async fn call(relay: &Relay, method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router(relay.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024).await.unwrap();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, value)
}

fn task(origin: &str, id: i64) -> Ref {
    Ref { kind: "task".into(), origin: origin.into(), id }
}

/// A field change written by `tag` at outbox seq `seq`.
fn change(tag: &str, seq: i64, entity: Ref, subject: Option<Ref>, field: &str, value: Value) -> Change {
    Change {
        cid: format!("{tag}:{seq}"),
        entity,
        subject,
        field: Some(field.into()),
        value,
        clock: Clock { wall_ms: 1_000 + seq, counter: 0, tag: tag.into() },
    }
}

async fn push(f: &Fixture, key: &str, room: &str, changes: Vec<Change>) -> (StatusCode, Value) {
    call(&f.relay, "POST", &format!("/v1/rooms/{room}/changes"), Some(key), Some(json!({ "changes": changes }))).await
}

async fn pull(f: &Fixture, key: &str, room: &str, since: i64) -> Value {
    let (s, v) = call(&f.relay, "GET", &format!("/v1/rooms/{room}/changes?since={since}"), Some(key), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v
}

async fn join(f: &Fixture, key: &str, room: &str) -> Value {
    let (s, v) = call(&f.relay, "POST", &format!("/v1/rooms/{room}/join"), Some(key), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v
}

#[tokio::test]
async fn only_hello_answers_without_a_key() {
    let f = fixture();
    let (s, v) = call(&f.relay, "GET", "/v1/hello", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["protocol_max"], 1);

    for (method, uri) in [("GET", "/v1/whoami"), ("GET", "/v1/rooms"), ("POST", "/v1/rooms/x/join"), ("GET", "/v1/rooms/x/changes")] {
        let (s, _) = call(&f.relay, method, uri, None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} {uri} without a key");
        let (s, _) = call(&f.relay, method, uri, Some("akb_nope"), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} {uri} with an unknown key");
    }
}

#[tokio::test]
async fn whoami_names_the_user_and_tag_behind_a_key() {
    let f = fixture();
    let (s, v) = call(&f.relay, "GET", "/v1/whoami", Some(&f.benny), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["user"], "benny");
    assert_eq!(v["tag"], "b1");
    assert_eq!(v["high_water"], json!({ "outbox": 0, "task": 0, "note": 0, "event": 0 }));
}

#[tokio::test]
async fn tags_are_validated_reserved_and_never_reused() {
    let db = ServerDb::open_in_memory().unwrap();
    db.add_user("benny").unwrap();
    db.add_user("volker").unwrap();
    assert!(db.add_key("benny", "b-1", "").is_err(), "'-' would break handle parsing");
    assert!(db.add_key("benny", "srv", "").is_err(), "the server's own tag");
    db.add_key("benny", "b1", "").unwrap();
    let err = db.add_key("volker", "b1", "").unwrap_err().to_string();
    assert!(err.contains("already belongs to benny"), "{err}");
}

#[tokio::test]
async fn revoked_keys_and_disabled_users_are_locked_out_but_rotation_keeps_the_tag() {
    let f = fixture();
    let new_key = f.relay.db.lock().unwrap().rotate_key("b1").unwrap();
    let (s, _) = call(&f.relay, "GET", "/v1/whoami", Some(&f.benny), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "the rotated-away key is dead");
    let (s, v) = call(&f.relay, "GET", "/v1/whoami", Some(&new_key), None).await;
    assert_eq!((s, v["tag"].clone()), (StatusCode::OK, json!("b1")), "same tag under the new key");

    f.relay.db.lock().unwrap().disable_user("volker").unwrap();
    let (s, _) = call(&f.relay, "GET", "/v1/whoami", Some(&f.volker), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn joining_creates_a_room_once_and_lists_its_members() {
    let f = fixture();
    let first = join(&f, &f.benny, "Sbahn").await;
    assert_eq!(first["created"], true);
    assert_eq!(first["room"]["name"], "sbahn", "one spelling, whatever was typed");
    let second = join(&f, &f.volker, "sbahn").await;
    assert_eq!(second["created"], false);

    let (_, rooms) = call(&f.relay, "GET", "/v1/rooms", Some(&f.benny), None).await;
    let members: Vec<&str> = rooms[0]["members"].as_array().unwrap().iter().map(|m| m["tag"].as_str().unwrap()).collect();
    assert_eq!(members, ["b1", "v1"]);
    assert_eq!(rooms[0]["created_by"], "benny");
}

#[tokio::test]
async fn reading_and_writing_need_membership() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let (s, _) = push(&f, &f.volker, "sbahn", vec![change("v1", 1, task("v1", 1), None, "title", json!("x"))]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "volker has not joined");
    let (s, _) = call(&f.relay, "GET", "/v1/rooms/sbahn/changes", Some(&f.volker), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_pushed_change_reaches_the_other_member_with_its_author() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    join(&f, &f.volker, "sbahn").await;

    let (s, v) = push(&f, &f.benny, "sbahn", vec![
        change("b1", 1, task("b1", 812), Some(task("b1", 812)), "title", json!("auth timeout")),
        change("b1", 2, task("b1", 812), Some(task("b1", 812)), "status", json!("doing")),
    ]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["accepted"], 2);

    let page = pull(&f, &f.volker, "sbahn", 0).await;
    let got = page["changes"].as_array().unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0]["author"], "benny", "the author comes from the key");
    assert_eq!(got[1]["change"]["value"], "doing");
    assert_eq!(page["head"], got[1]["seq"]);

    let rest = pull(&f, &f.volker, "sbahn", page["head"].as_i64().unwrap()).await;
    assert!(rest["changes"].as_array().unwrap().is_empty(), "nothing past the cursor");
}

#[tokio::test]
async fn pulls_page_and_say_when_there_is_more() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let batch: Vec<Change> = (1..=5).map(|i| change("b1", i, task("b1", i), None, "title", json!(i))).collect();
    push(&f, &f.benny, "sbahn", batch).await;

    let (_, page) = call(&f.relay, "GET", "/v1/rooms/sbahn/changes?since=0&limit=2", Some(&f.benny), None).await;
    assert_eq!(page["changes"].as_array().unwrap().len(), 2);
    assert_eq!(page["more"], true);
    let last = page["changes"][1]["seq"].as_i64().unwrap();
    let (_, page) = call(&f.relay, "GET", &format!("/v1/rooms/sbahn/changes?since={last}&limit=10"), Some(&f.benny), None).await;
    assert_eq!(page["changes"].as_array().unwrap().len(), 3);
    assert_eq!(page["more"], false);
}

#[tokio::test]
async fn a_retried_push_is_stored_once() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let batch = vec![change("b1", 1, task("b1", 1), None, "title", json!("x"))];
    push(&f, &f.benny, "sbahn", batch.clone()).await;
    let (s, v) = push(&f, &f.benny, "sbahn", batch).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["accepted"].clone(), v["duplicates"].clone()), (json!(0), json!(1)));
    assert_eq!(pull(&f, &f.benny, "sbahn", 0).await["changes"].as_array().unwrap().len(), 1);
}

/// A store restored from a backup reuses its outbox seqs. The same change id with different
/// content is that, not a retry -- dropping it as a duplicate would silently lose new work.
#[tokio::test]
async fn a_reused_change_id_with_new_content_is_refused_and_nothing_is_stored() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    push(&f, &f.benny, "sbahn", vec![change("b1", 1, task("b1", 1), None, "title", json!("old"))]).await;
    let (s, v) = push(&f, &f.benny, "sbahn", vec![
        change("b1", 2, task("b1", 2), None, "title", json!("fine")),
        change("b1", 1, task("b1", 1), None, "title", json!("restored store")),
    ]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"]["message"].as_str().unwrap().contains("restored"), "{v}");
    assert_eq!(pull(&f, &f.benny, "sbahn", 0).await["changes"].as_array().unwrap().len(), 1, "all or nothing");
}

#[tokio::test]
async fn nobody_writes_under_another_tag() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let mut wrong_cid = change("b1", 1, task("b1", 1), None, "title", json!("x"));
    wrong_cid.cid = "v1:1".into();
    let (s, _) = push(&f, &f.benny, "sbahn", vec![wrong_cid]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "change id under volker's tag");

    let mut wrong_clock = change("b1", 1, task("b1", 1), None, "title", json!("x"));
    wrong_clock.clock.tag = "v1".into();
    let (s, _) = push(&f, &f.benny, "sbahn", vec![wrong_clock]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a clock with volker's tag would win his ties");

    // Editing someone else's task is normal: only the change's own id and clock must be yours.
    let (s, _) = push(&f, &f.benny, "sbahn", vec![change("b1", 1, task("v1", 122), Some(task("v1", 122)), "status", json!("doing"))]).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn high_water_tracks_the_counters_a_restored_store_would_reissue() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    push(&f, &f.benny, "sbahn", vec![
        change("b1", 7, task("b1", 812), None, "title", json!("mine")),
        change("b1", 9, task("v1", 999), None, "status", json!("doing")),
        change("b1", 8, Ref { kind: "note".into(), origin: "b1".into(), id: 40 }, None, "body", json!("n")),
    ]).await;
    let (_, v) = call(&f.relay, "GET", "/v1/whoami", Some(&f.benny), None).await;
    assert_eq!(
        v["high_water"],
        json!({ "outbox": 9, "task": 812, "note": 40, "event": 0 }),
        "only entities benny created raise his id marks; volker's #999 does not"
    );
}

#[tokio::test]
async fn forget_removes_a_task_everywhere_but_keeps_its_notes_and_cannot_be_undone_by_a_late_push() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    join(&f, &f.volker, "sbahn").await;
    let t = task("b1", 812);
    let note = Ref { kind: "note".into(), origin: "b1".into(), id: 40 };
    let event = |id| Ref { kind: "event".into(), origin: "b1".into(), id };
    push(&f, &f.benny, "sbahn", vec![
        change("b1", 1, t.clone(), Some(t.clone()), "title", json!("secret title")),
        change("b1", 2, event(5), Some(t.clone()), "body", json!("task history")),
        change("b1", 3, note.clone(), Some(note.clone()), "body", json!("what was learned")),
        // A note's own event sits on the task too, but its subject is the note: forgetting the
        // task detaches notes and their history, as core::forget does locally.
        change("b1", 4, event(6), Some(note.clone()), "body", json!("note history")),
    ]).await;

    f.relay.db.lock().unwrap().forget("sbahn", &t).unwrap();

    let page = pull(&f, &f.volker, "sbahn", 0).await;
    let bodies: Vec<String> = page["changes"].as_array().unwrap().iter().map(|c| c["change"]["value"].to_string()).collect();
    assert!(!bodies.iter().any(|b| b.contains("secret") || b.contains("task history")), "{bodies:?}");
    assert!(bodies.iter().any(|b| b.contains("what was learned")));
    assert!(bodies.iter().any(|b| b.contains("note history")));
    let tomb = page["changes"].as_array().unwrap().last().unwrap();
    assert_eq!(tomb["change"]["field"], "forgotten");
    assert_eq!(tomb["change"]["entity"]["id"], 812);
    assert_eq!(tomb["change"]["clock"]["tag"], "srv");

    // Volker was offline and still holds an edit to the task.
    let (s, v) = push(&f, &f.volker, "sbahn", vec![change("v1", 1, t.clone(), Some(t.clone()), "title", json!("secret title"))]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["accepted"].clone(), v["forgotten"].clone()), (json!(0), json!(1)));
}

#[tokio::test]
async fn empty_rooms_go_after_seven_days_but_never_a_room_with_content_or_members() {
    let f = fixture();
    join(&f, &f.benny, "typo").await;
    join(&f, &f.benny, "kept").await;
    join(&f, &f.benny, "connected").await;
    push(&f, &f.benny, "kept", vec![change("b1", 1, task("b1", 1), None, "title", json!("x"))]).await;
    for room in ["typo", "kept"] {
        call(&f.relay, "POST", &format!("/v1/rooms/{room}/leave"), Some(&f.benny), None).await;
    }
    {
        let db = f.relay.db.lock().unwrap();
        let old = ai_kanban::core::now() - 8 * 86400;
        db.conn.execute("UPDATE rooms SET created_at = ?1", [old]).unwrap();
        db.conn.execute("UPDATE members SET joined_at = ?1, left_at = CASE WHEN left_at IS NULL THEN NULL ELSE ?1 END", [old]).unwrap();
        assert_eq!(db.remove_empty_rooms(ai_kanban::server::EMPTY_ROOM_AGE).unwrap(), ["typo"]);
    }
    let (_, rooms) = call(&f.relay, "GET", "/v1/rooms", Some(&f.benny), None).await;
    let names: Vec<&str> = rooms.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["connected", "kept"]);
}

#[test]
fn the_server_refuses_to_open_a_personal_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kanban.db");
    ai_kanban::core::Store::open(&path).unwrap();
    let err = ServerDb::open(&path).err().expect("a kanban store must not open as a relay db").to_string();
    assert!(err.contains("not an ai-kanban server database"), "{err}");
}

/// Notifications come from polling, so a change made by another process -- an admin command
/// run with `docker exec` -- reaches a connected client too.
#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_announces_changes_made_by_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let (key, relay) = {
        let db = ServerDb::open(&path).unwrap();
        db.add_user("benny").unwrap();
        let key = db.add_key("benny", "b1", "").unwrap();
        (key, Relay::new(db))
    };
    let p = relay.db.lock().unwrap().authenticate(&key).unwrap();
    relay.db.lock().unwrap().join("sbahn", &p).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(relay)).await.unwrap() });

    let stream_key = key.clone();
    let reader = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        write!(s, "GET /v1/rooms/sbahn/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {stream_key}\r\n\r\n").unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while seen.matches("event: head").count() < 2 {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "stream ended early: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        seen
    });

    // A second handle on the same file stands in for `docker exec ai-kanban server ...`.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let other = ServerDb::open(&path).unwrap();
    let p = other.authenticate(&key).unwrap();
    other.push("sbahn", &p, &[change("b1", 1, task("b1", 1), None, "title", json!("x"))]).unwrap();

    let seen = reader.await.unwrap();
    assert!(seen.contains("data: 0"), "the first event is the head at connect: {seen}");
    assert!(seen.contains("data: 1"), "then the head after the other process wrote: {seen}");
}
