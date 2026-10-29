//! `--debug`: what it logs, that it never logs a key, and that tapping the traffic does not
//! change it -- above all that the event stream still streams, since the tap buffers every
//! other response whole.

use ai_kanban_protocol::{Change, Clock, Ref};
use ai_kanban_relay::{router, Relay, ServerDb};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

type Lines = Arc<Mutex<Vec<String>>>;

fn traced(db: ServerDb) -> (Relay, Lines) {
    let lines: Lines = Default::default();
    let sink = lines.clone();
    (Relay::new(db).with_trace(Arc::new(move |l| sink.lock().unwrap().push(l))), lines)
}

fn fixture() -> (Relay, Lines, String) {
    let db = ServerDb::open_in_memory().unwrap();
    let key = db.add_key("benny", "b1").unwrap();
    let (relay, lines) = traced(db);
    (relay, lines, key)
}

async fn call(relay: &Relay, method: &str, uri: &str, key: Option<&str>, body: Option<String>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b)).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router(relay.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn log(lines: &Lines) -> String {
    lines.lock().unwrap().join("\n")
}

#[tokio::test]
async fn a_push_and_a_pull_are_logged_both_ways_and_still_work() {
    let (relay, lines, key) = fixture();
    call(&relay, "POST", "/v1/rooms/sbahn/join", Some(&key), None).await;
    let change = Change {
        cid: "b1:1".into(),
        entity: Ref { kind: "task".into(), origin: "b1".into(), id: 1 },
        subject: None,
        field: Some("title".into()),
        value: json!("Fix the timetable"),
        clock: Clock { wall_ms: 1_000, counter: 0, tag: "b1".into() },
        extra: Default::default(),
    };
    let (s, v) = call(&relay, "POST", "/v1/rooms/sbahn/changes", Some(&key), Some(json!({ "changes": [change] }).to_string())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, page) = call(&relay, "GET", "/v1/rooms/sbahn/changes?since=0", Some(&key), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page["changes"][0]["change"]["value"], "Fix the timetable", "the response reaches the client whole: {page}");

    let log = log(&lines);
    assert!(log.contains("> POST /v1/rooms/sbahn/changes benny/b1"), "{log}");
    assert!(log.contains("Fix the timetable"), "bodies are shown: {log}");
    assert!(log.contains("> GET /v1/rooms/sbahn/changes?since=0 benny/b1"), "{log}");
    assert!(log.matches("< 200").count() >= 3, "{log}");
    assert!(!log.contains(&key), "a key never appears in the log: {log}");
}

#[tokio::test]
async fn what_the_router_turns_away_is_logged_too() {
    let (relay, lines, key) = fixture();
    let (s, _) = call(&relay, "GET", "/v1/whoami", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let wrong = "akb_0000000000000000000000000000000000000000000000000000000000001234";
    call(&relay, "GET", "/v1/whoami", Some(wrong), None).await;
    let (s, _) = call(&relay, "POST", "/v1/rooms/sbahn/changes", Some(&key), Some("{not json".into())).await;
    assert!(s.is_client_error(), "{s}");

    let log = log(&lines);
    assert!(log.contains("> GET /v1/whoami (no key)"), "{log}");
    assert!(log.contains("(unknown key ...1234)") && !log.contains(wrong), "only the key's last four: {log}");
    assert!(log.contains("< 401"), "{log}");
    assert!(log.contains("{not json"), "the malformed body, which no handler ever saw: {log}");
    assert!(log.contains(&format!("< {}", s.as_u16())), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_still_streams_and_logs_open_heads_and_close() {
    let (relay, lines, key) = fixture();
    let p = relay.db.lock().unwrap().authenticate(&key).unwrap();
    relay.db.lock().unwrap().join("sbahn", &p).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(relay)).await.unwrap() });

    let seen = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        write!(s, "GET /v1/rooms/sbahn/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\n\r\n").unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while !seen.contains("data: 0") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "stream ended early: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        seen
        // The socket closes here: the client going away.
    })
    .await
    .unwrap();
    assert!(seen.contains("event: head"), "{seen}");

    // The relay notices the closed socket on the stream's next write, the tick after.
    let mut log = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        log = self::log(&lines);
        if log.contains("sse closed") {
            break;
        }
    }
    assert!(log.contains("< 200") && log.contains("event stream"), "{log}");
    assert!(log.contains("sse open benny/b1 room sbahn"), "{log}");
    assert!(log.contains("sse head 0"), "{log}");
    assert!(log.contains("sse closed after") && log.contains("client went away"), "{log}");
}

/// Debug mode is when people test; watching must not change what happens. Logging a request
/// from an unnamed key once ran the authentication that gives it a generated name -- and its
/// tag -- before the client's own name arrived.
#[tokio::test]
async fn logging_a_request_does_not_name_the_key() {
    let db = ServerDb::open_in_memory().unwrap();
    let (_, key) = db.issue_key("volker laptop").unwrap();
    let (relay, lines) = traced(db);
    let (s, v) = call(&relay, "PUT", "/v1/me", Some(&key), Some(json!({ "name": "Volker" }).to_string())).await;
    assert_eq!((s, v["tag"].clone()), (StatusCode::OK, json!("v1")), "the tag comes from the chosen name: {v}");
    assert!(log(&lines).contains("not named yet"), "{}", log(&lines));
}
