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
    // Errors the router produces itself (413, 415) are plain text, not the relay's JSON.
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
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
        extra: Default::default(),
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

/// The stream re-checks access on every tick, like every other route checks it per request.
#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_ends_when_its_key_is_revoked() {
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
    let key_id = p.key_id;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(relay)).await.unwrap() });

    let reader = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        write!(s, "GET /v1/rooms/sbahn/events HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {key}\r\n\r\n").unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        loop {
            match s.read(&mut buf) {
                Ok(0) => return (true, seen),
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(_) => return (false, seen),
            }
            if seen.ends_with("\r\n0\r\n\r\n") {
                return (true, seen);
            }
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ServerDb::open(&path).unwrap().revoke_key(key_id).unwrap();

    let (ended, seen) = reader.await.unwrap();
    assert!(seen.contains("event: head"), "{seen}");
    assert!(ended, "the stream kept running after its key was revoked: {seen}");
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

fn db_with_benny() -> (ServerDb, String) {
    let db = ServerDb::open_in_memory().unwrap();
    db.add_user("benny").unwrap();
    let key = db.add_key("benny", "b1", "").unwrap();
    (db, key)
}

#[test]
fn admin_commands_refuse_what_does_not_exist_or_already_happened() {
    let (db, _) = db_with_benny();
    assert!(db.add_user("benny").unwrap_err().to_string().contains("exists"));
    for bad in ["", "Benny", "a b", "-x"] {
        assert!(db.add_user(bad).is_err(), "user name {bad:?}");
    }
    assert!(db.add_key("nobody", "n1", "").unwrap_err().to_string().contains("no user"));
    assert!(db.rotate_key("zz").unwrap_err().to_string().contains("no tag"));

    let key_id = db.keys(Some("benny")).unwrap()[0].id;
    db.revoke_key(key_id).unwrap();
    assert!(db.revoke_key(key_id).is_err(), "revoking twice");
    db.disable_user("benny").unwrap();
    assert!(db.disable_user("benny").is_err(), "disabling twice");
    assert!(db.users().unwrap()[0].disabled_at.is_some());
}

#[test]
fn a_key_lists_its_user_tag_label_and_state() {
    let (db, key) = db_with_benny();
    db.authenticate(&key).unwrap();
    let k = &db.keys(None).unwrap()[0];
    assert_eq!((k.user.as_str(), k.tag.as_str(), k.revoked_at), ("benny", "b1", None));
    assert!(k.last_used_at.is_some(), "authenticating records when the key was last seen");
    assert_eq!(db.users().unwrap()[0].tags, ["b1"]);
}

#[tokio::test]
async fn the_bearer_scheme_is_case_insensitive_and_other_schemes_are_refused() {
    let f = fixture();
    for header in [format!("bearer {}", f.benny), format!("BEARER {}", f.benny), format!("Bearer  {} ", f.benny)] {
        let req = Request::builder().uri("/v1/whoami").header("authorization", header.clone()).body(Body::empty()).unwrap();
        let res = router(f.relay.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{header:?}");
    }
    let req = Request::builder().uri("/v1/whoami").header("authorization", format!("Basic {}", f.benny)).body(Body::empty()).unwrap();
    assert_eq!(router(f.relay.clone()).oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn room_names_are_validated_and_have_one_spelling_everywhere() {
    let f = fixture();
    for bad in ["-bad", "a%20b", &"x".repeat(65)] {
        let (s, _) = call(&f.relay, "POST", &format!("/v1/rooms/{bad}/join"), Some(&f.benny), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "room {bad:?}");
    }
    join(&f, &f.benny, "sbahn").await;
    let (s, v) = push(&f, &f.benny, "SBAHN", vec![change("b1", 1, task("b1", 1), None, "title", json!("x"))]).await;
    assert_eq!(s, StatusCode::OK, "uppercase reaches the same room: {v}");
    assert_eq!(pull(&f, &f.benny, "Sbahn", 0).await["changes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn leaving_locks_a_member_out_and_rejoining_brings_the_history_back() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    push(&f, &f.benny, "sbahn", vec![change("b1", 1, task("b1", 1), None, "title", json!("x"))]).await;
    let (s, _) = call(&f.relay, "POST", "/v1/rooms/sbahn/leave", Some(&f.benny), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&f.relay, "POST", "/v1/rooms/sbahn/leave", Some(&f.benny), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "leaving twice");
    let (s, _) = push(&f, &f.benny, "sbahn", vec![change("b1", 2, task("b1", 2), None, "title", json!("y"))]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a member who left cannot push");

    let again = join(&f, &f.benny, "sbahn").await;
    assert_eq!(again["created"], false);
    assert_eq!(pull(&f, &f.benny, "sbahn", 0).await["changes"].as_array().unwrap().len(), 1, "history survived the leave");
}

#[tokio::test]
async fn a_deleted_room_is_gone_and_joining_its_name_starts_empty() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    push(&f, &f.benny, "sbahn", vec![change("b1", 1, task("b1", 1), None, "title", json!("x"))]).await;
    {
        let db = f.relay.db.lock().unwrap();
        let r = db.room_removal("sbahn").unwrap();
        assert_eq!((r.changes, r.members), (1, 1));
        db.delete_room("sbahn").unwrap();
        assert!(db.delete_room("sbahn").is_err());
    }
    let (s, _) = call(&f.relay, "GET", "/v1/rooms/sbahn/changes", Some(&f.benny), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let fresh = join(&f, &f.benny, "sbahn").await;
    assert_eq!((fresh["created"].clone(), fresh["room"]["changes"].clone()), (json!(true), json!(0)));
}

#[tokio::test]
async fn malformed_changes_are_refused_with_the_reason() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let ok = || change("b1", 1, task("b1", 1), None, "title", json!("x"));
    let cases: Vec<(&str, Change)> = vec![
        ("unknown kind", { let mut c = ok(); c.entity.kind = "board".into(); c }),
        ("zero id", { let mut c = ok(); c.entity.id = 0; c }),
        ("bad origin", { let mut c = ok(); c.entity.origin = "B-1".into(); c }),
        ("event subject", { let mut c = ok(); c.subject = Some(Ref { kind: "event".into(), origin: "b1".into(), id: 1 }); c }),
        ("cid without seq", { let mut c = ok(); c.cid = "b1".into(); c }),
        ("cid with text seq", { let mut c = ok(); c.cid = "b1:x".into(); c }),
        ("cid with negative seq", { let mut c = ok(); c.cid = "b1:-3".into(); c }),
    ];
    for (what, c) in cases {
        let (s, v) = push(&f, &f.benny, "sbahn", vec![c]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().starts_with("change 0"), "{what}: the message names the change: {v}");
    }
    assert_eq!(pull(&f, &f.benny, "sbahn", 0).await["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn batches_have_a_size_limit_and_an_empty_batch_is_fine() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let (s, v) = push(&f, &f.benny, "sbahn", vec![]).await;
    assert_eq!((s, v["accepted"].clone()), (StatusCode::OK, json!(0)));

    let too_many: Vec<Change> = (1..=1001).map(|i| change("b1", i, task("b1", i), None, "t", json!(i))).collect();
    let (s, v) = push(&f, &f.benny, "sbahn", too_many).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"]["message"].as_str().unwrap().contains("limit is 1000"), "{v}");

    let huge = json!({ "changes": [], "padding": "x".repeat(9 * 1024 * 1024) });
    let (s, _) = call(&f.relay, "POST", "/v1/rooms/sbahn/changes", Some(&f.benny), Some(huge)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn the_same_change_twice_in_one_batch_is_one_retry_but_different_content_is_refused() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let c = change("b1", 1, task("b1", 1), None, "title", json!("x"));
    let (_, v) = push(&f, &f.benny, "sbahn", vec![c.clone(), c.clone()]).await;
    assert_eq!((v["accepted"].clone(), v["duplicates"].clone()), (json!(1), json!(1)));

    let mut other = change("b1", 2, task("b1", 2), None, "title", json!("a"));
    let mut clash = other.clone();
    clash.value = json!("b");
    let (s, _) = push(&f, &f.benny, "sbahn", vec![other.clone(), clash]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    other.cid = "b1:3".into();
    assert_eq!(pull(&f, &f.benny, "sbahn", 0).await["changes"].as_array().unwrap().len(), 1, "the refused batch left nothing");
}

/// What a newer client adds must reach the other members through a relay that does not know it.
#[tokio::test]
async fn values_and_unknown_envelope_fields_pass_through_untouched() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    join(&f, &f.volker, "sbahn").await;
    let mut c = change("b1", 1, task("b1", 1), None, "repos", json!({ "list": ["git:a", "git:b"], "nested": { "n": [1, 2.5, null, true] } }));
    c.extra.insert("priority_hint".into(), json!("urgent"));
    let (s, _) = push(&f, &f.benny, "sbahn", vec![c.clone()]).await;
    assert_eq!(s, StatusCode::OK);
    let got: Change = serde_json::from_value(pull(&f, &f.volker, "sbahn", 0).await["changes"][0]["change"].clone()).unwrap();
    assert_eq!(got, c);

    // And a retry of it is still recognised as one.
    let (_, v) = push(&f, &f.benny, "sbahn", vec![c]).await;
    assert_eq!(v["duplicates"], 1);
}

#[tokio::test]
async fn pull_cursors_and_limits_are_clamped_not_rejected() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    push(&f, &f.benny, "sbahn", (1..=3).map(|i| change("b1", i, task("b1", i), None, "t", json!(i))).collect()).await;

    let past = pull(&f, &f.benny, "sbahn", 999).await;
    assert!(past["changes"].as_array().unwrap().is_empty());
    assert_eq!(past["head"], 3, "the head is reported even past the end");
    let (_, neg) = call(&f.relay, "GET", "/v1/rooms/sbahn/changes?since=-5", Some(&f.benny), None).await;
    assert_eq!(neg["changes"].as_array().unwrap().len(), 3);
    let (_, zero) = call(&f.relay, "GET", "/v1/rooms/sbahn/changes?limit=0", Some(&f.benny), None).await;
    assert_eq!((zero["changes"].as_array().unwrap().len(), zero["more"].clone()), (1, json!(true)));
}

#[tokio::test]
async fn high_water_never_goes_down() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    push(&f, &f.benny, "sbahn", vec![change("b1", 50, task("b1", 900), None, "t", json!(1))]).await;
    // A lower seq for another room's backlog is legitimate and must not lower the marks.
    push(&f, &f.benny, "sbahn", vec![change("b1", 3, task("b1", 4), None, "t", json!(2))]).await;
    let (_, v) = call(&f.relay, "GET", "/v1/whoami", Some(&f.benny), None).await;
    assert_eq!((v["high_water"]["outbox"].clone(), v["high_water"]["task"].clone()), (json!(50), json!(900)));
}

#[tokio::test]
async fn forgetting_a_note_removes_it_and_its_history_but_not_the_task() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let t = task("b1", 1);
    let n = Ref { kind: "note".into(), origin: "b1".into(), id: 7 };
    push(&f, &f.benny, "sbahn", vec![
        change("b1", 1, t.clone(), Some(t.clone()), "title", json!("task stays")),
        change("b1", 2, n.clone(), Some(n.clone()), "body", json!("note goes")),
        change("b1", 3, Ref { kind: "event".into(), origin: "b1".into(), id: 9 }, Some(n.clone()), "body", json!("note history goes")),
    ]).await;
    let removed = f.relay.db.lock().unwrap().forget("sbahn", &n).unwrap();
    assert_eq!(removed.changes, 2);
    let bodies: Vec<String> = pull(&f, &f.benny, "sbahn", 0).await["changes"].as_array().unwrap()
        .iter().map(|c| c["change"]["value"].to_string()).collect();
    assert!(bodies.iter().any(|b| b.contains("task stays")));
    assert!(!bodies.iter().any(|b| b.contains("note")), "{bodies:?}");

    let (_, v) = push(&f, &f.benny, "sbahn", vec![change("b1", 4, Ref { kind: "event".into(), origin: "b1".into(), id: 10 }, Some(n), "body", json!("late"))]).await;
    assert_eq!(v["forgotten"], 1, "a late change whose subject is the forgotten note is dropped too");
}

#[tokio::test]
async fn forgetting_is_idempotent_and_refuses_what_cannot_be_forgotten() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let t = task("b1", 1);
    push(&f, &f.benny, "sbahn", vec![change("b1", 1, t.clone(), Some(t.clone()), "title", json!("x"))]).await;
    let db = f.relay.db.lock().unwrap();
    db.forget("sbahn", &t).unwrap();
    let head = db.room_info(1).unwrap().head;
    assert_eq!(db.forget("sbahn", &t).unwrap().changes, 0);
    assert_eq!(db.room_info(1).unwrap().head, head, "no second tombstone");
    assert!(db.forget("sbahn", &Ref { kind: "event".into(), origin: "b1".into(), id: 1 }).is_err());
    assert!(db.forget("nope", &t).unwrap_err().to_string().contains("no room"));
}

#[test]
fn snapshots_are_consistent_copies_and_only_the_newest_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let db = ServerDb::open(&dir.path().join("server.db")).unwrap();
    db.add_user("benny").unwrap();
    let snaps = dir.path().join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    for t in [100, 200, 300] {
        std::fs::write(snaps.join(format!("server-{t}.db")), b"old").unwrap();
    }
    std::fs::write(snaps.join("unrelated.txt"), b"keep me").unwrap();
    let newest = ai_kanban::server::take_snapshot(&db, &snaps, 2).unwrap();

    let mut left: Vec<String> = std::fs::read_dir(&snaps).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    assert_eq!(left.len(), 3, "{left:?}");
    assert!(left.contains(&"server-300.db".to_string()) && left.contains(&"unrelated.txt".to_string()), "{left:?}");
    let copy = ServerDb::open(&newest).unwrap();
    assert_eq!(copy.users().unwrap()[0].name, "benny", "the snapshot is a working relay database");
    assert!(db.snapshot(&newest).is_err(), "never overwrites");
}

#[tokio::test]
async fn an_empty_room_survives_until_seven_days_after_it_was_last_used() {
    let f = fixture();
    join(&f, &f.benny, "young").await;
    join(&f, &f.benny, "recently-left").await;
    for room in ["young", "recently-left"] {
        call(&f.relay, "POST", &format!("/v1/rooms/{room}/leave"), Some(&f.benny), None).await;
    }
    let db = f.relay.db.lock().unwrap();
    let old = ai_kanban::core::now() - 30 * 86400;
    db.conn.execute("UPDATE rooms SET created_at = ?1 WHERE name = 'recently-left'", [old]).unwrap();
    assert!(db.remove_empty_rooms(ai_kanban::server::EMPTY_ROOM_AGE).unwrap().is_empty(),
        "one is new, the other was left just now");
}

#[tokio::test]
async fn the_event_stream_needs_a_key_and_membership() {
    let f = fixture();
    join(&f, &f.benny, "sbahn").await;
    let (s, _) = call(&f.relay, "GET", "/v1/rooms/sbahn/events", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&f.relay, "GET", "/v1/rooms/sbahn/events", Some(&f.volker), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
