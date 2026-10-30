//! The MCP process's background sync (`remote::daemon`), against a real relay over HTTP.
//!
//! One test, alone in its binary: the syncer reads its relay and store from the environment,
//! which is process-wide, and no other test may see these variables.

mod common;
use common::relay_http::HttpRelay;

use ai_kanban::core::model::*;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;
use ai_kanban::remote::{sync_board, Client};
use std::time::{Duration, Instant};

fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_session_publishes_its_writes_and_receives_a_teammates_without_being_asked() {
    let mut relay = HttpRelay::start();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("kanban.db");
    let key = relay.key("benny", "b1");
    // SAFETY: this binary holds exactly one test, so nothing else reads the environment.
    unsafe {
        std::env::set_var("AI_KANBAN_DB", &db);
        std::env::set_var("AI_KANBAN_SERVER_URL", &relay.url);
        std::env::set_var("AI_KANBAN_SERVER_KEY", &key);
        std::env::remove_var("AI_KANBAN_SYNC");
    }
    let me = Store::open(&db).unwrap();
    me.set_origin_tag("b1").unwrap();
    let pid = me.create_board("sbb").unwrap().id;
    Client::new(&relay.url, &key, Duration::from_secs(5)).join("sbahn", None).unwrap();
    me.connect_board(pid, "sbahn").unwrap();

    let volker_key = relay.key("volker", "v1");
    let volker_client = Client::new(&relay.url, &volker_key, Duration::from_secs(5));
    volker_client.join("sbahn", None).unwrap();
    let volker = Store::open_in_memory().unwrap();
    volker.set_origin_tag("v1").unwrap();
    let vp = volker.create_board("x").unwrap().id;
    volker.connect_board(vp, "sbahn").unwrap();

    let syncer = ai_kanban::remote::daemon::Syncer::start().expect("a remote is set, so the syncer starts");

    // My write reaches the room without anyone running a sync.
    me.create_task(pid, TaskDraft::new("mine")).unwrap();
    syncer.kick();
    until("my task on the relay", Duration::from_secs(10), || {
        let _ = sync_board(&volker, &volker_client, vp);
        volker.board(vp, &BoardQuery::board()).unwrap().tasks.iter().any(|t| t.title == "mine")
    });

    // A teammate's push reaches me through the notification stream, well inside the 60s backstop.
    volker.create_task(vp, TaskDraft::new("theirs")).unwrap();
    sync_board(&volker, &volker_client, vp).unwrap();
    until("their task here", Duration::from_secs(15), || {
        me.board(pid, &BoardQuery::board()).unwrap().tasks.iter().any(|t| t.title == "theirs")
    });
    assert!(me.sync_status(pid).unwrap().unwrap().members.len() == 2, "the header learns the team");

    // A round set off by a write only moves the change: the identity checks and the member list
    // ran in the startup round and wait for the minute's full one. From across the world each
    // of them was ~0.3s on every change's trip (note #333).
    relay.log.lock().unwrap().clear();
    me.create_task(pid, TaskDraft::new("quick")).unwrap();
    syncer.kick();
    until("the quick task on the relay", Duration::from_secs(10), || {
        let _ = sync_board(&volker, &volker_client, vp);
        volker.board(vp, &BoardQuery::board()).unwrap().tasks.iter().any(|t| t.title == "quick")
    });
    let mine: Vec<String> = relay.log.lock().unwrap().iter().filter(|l| l.contains(" > ")).filter(|l| l.contains("benny/b1") || l.contains("/v1/hello")).cloned().collect();
    assert!(mine.iter().any(|l| l.contains("> POST /v1/rooms/sbahn/changes")), "the write was pushed: {mine:#?}");
    for skipped in ["/v1/hello", "/v1/whoami", "/v1/me ", "/v1/rooms benny"] {
        assert!(!mine.iter().any(|l| l.contains(skipped)), "a write's round asked {skipped}: {mine:#?}");
    }
    assert_eq!(me.sync_status(pid).unwrap().unwrap().members.len(), 2, "a round that did not ask keeps the team");

    // A kick with nothing to send runs no round: the page's poller kicks on every change it
    // sees, including the write the last round already sent and the pull it just applied.
    relay.log.lock().unwrap().clear();
    syncer.kick();
    syncer.kick_now();
    std::thread::sleep(Duration::from_millis(1500));
    let asked: Vec<String> = relay.log.lock().unwrap().iter().filter(|l| l.contains(" > ") && l.contains("benny/b1")).cloned().collect();
    assert!(asked.is_empty(), "a kick with nothing to send went to the relay: {asked:#?}");

    // The round before a claim is bounded when the relay is gone, and does not fail.
    relay.stop();
    let start = Instant::now();
    syncer.now(&me, pid);
    assert!(start.elapsed() < Duration::from_secs(4), "took {:?}", start.elapsed());
    assert!(me.sync_status(pid).unwrap().unwrap().last_error.is_some(), "the header will say offline");
}
