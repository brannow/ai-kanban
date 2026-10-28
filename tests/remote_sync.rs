//! A client's sync round against the real relay over HTTP, including what happens when the
//! relay is gone, the key is revoked, the protocol does not match, or the store was restored
//! from a backup. Local work must never fail because of any of them.

mod common;
use common::relay_http::HttpRelay;

use ai_kanban::core::model::*;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;
use ai_kanban::remote::{check_identity, sync_board, Client, RemoteError};
use std::time::Duration;

struct Machine {
    store: Store,
    pid: i64,
    client: Client,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn machine(relay: &HttpRelay, user: &str, tag: &str, board: &str) -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kanban.db");
    let store = Store::open(&path).unwrap();
    let client = Client::new(&relay.url, &relay.key(user, tag), Duration::from_secs(5));
    store.set_origin_tag(&client.whoami().unwrap().tag).unwrap();
    let pid = store.create_board(board).unwrap().id;
    client.join("sbahn").unwrap();
    store.connect_board(pid, "sbahn").unwrap();
    Machine { store, pid, client, path, _dir: dir }
}

fn titles(m: &Machine) -> Vec<String> {
    let mut t: Vec<String> = m.store.board(m.pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    t.sort();
    t
}

#[test]
fn two_machines_meet_through_the_relay() {
    let relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    let volker = machine(&relay, "volker", "v1", "sbahn upgrade");
    benny.store.create_task(benny.pid, TaskDraft::new("benny's")).unwrap();
    volker.store.create_task(volker.pid, TaskDraft::new("volker's")).unwrap();
    for m in [&benny, &volker, &benny] {
        check_identity(&m.store, &m.client).unwrap();
        sync_board(&m.store, &m.client, m.pid).unwrap();
    }
    assert_eq!(titles(&benny), ["benny's", "volker's"]);
    assert_eq!(titles(&volker), ["benny's", "volker's"]);
    let s = benny.store.sync_status(benny.pid).unwrap().unwrap();
    assert_eq!((s.unsent, s.last_error), (0, None));
    assert!(s.last_ok_at.is_some());
    let tags: Vec<&str> = s.members.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(tags, ["b1", "v1"], "the room's members are recorded for the header");
}

#[test]
fn with_the_relay_gone_local_work_goes_on_and_the_backlog_drains_when_it_returns() {
    let mut relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    let volker = machine(&relay, "volker", "v1", "x");
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();

    relay.stop();
    benny.store.create_task(benny.pid, TaskDraft::new("written offline")).unwrap();
    let err = sync_board(&benny.store, &benny.client, benny.pid).unwrap_err();
    assert!(matches!(err, RemoteError::Offline(_)), "{err:?}");
    let s = benny.store.sync_status(benny.pid).unwrap().unwrap();
    assert!(s.unsent > 0 && s.last_error.is_some(), "{s:?}");
    assert_eq!(benny.store.paused().unwrap(), None, "being offline is not a pause");

    relay.resume();
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();
    sync_board(&volker.store, &volker.client, volker.pid).unwrap();
    assert_eq!(titles(&volker), ["written offline"]);
    let s = benny.store.sync_status(benny.pid).unwrap().unwrap();
    assert_eq!((s.unsent, s.last_error), (0, None));
}

#[test]
fn a_revoked_key_fails_the_round_with_a_clear_reason_and_local_work_goes_on() {
    let relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();
    let key = relay.admin().keys(Some("benny")).unwrap()[0].id;
    relay.admin().revoke_key(key).unwrap();

    benny.store.create_task(benny.pid, TaskDraft::new("still works")).unwrap();
    assert!(matches!(sync_board(&benny.store, &benny.client, benny.pid), Err(RemoteError::Unauthorized)));
    let s = benny.store.sync_status(benny.pid).unwrap().unwrap();
    assert!(s.last_error.unwrap().contains("rejected the key"));
    assert_eq!(titles(&benny), ["still works"]);
}

#[test]
fn a_key_for_another_tag_pauses_sync_until_it_is_fixed() {
    let relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    let other = Client::new(&relay.url, &relay.key("benny", "b9"), Duration::from_secs(5));
    let err = check_identity(&benny.store, &other).unwrap_err();
    assert!(matches!(err, RemoteError::Paused(ref why) if why.contains("origin tag b9")), "{err:?}");
    assert!(matches!(sync_board(&benny.store, &benny.client, benny.pid), Err(RemoteError::Paused(_))), "paused for the store, whatever the key");
    check_identity(&benny.store, &benny.client).unwrap();
    assert_eq!(benny.store.paused().unwrap(), None, "a good check lifts the pause");
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();
}

#[test]
fn a_store_that_never_published_takes_the_tag_of_its_first_key() {
    // A key arriving through the plugin's settings is saved without asking the relay anything,
    // so the store has no tag yet when sync first runs. Having never published, it has no
    // numbers under any tag to collide with: taking the key's tag is what `remote` would do.
    let relay = HttpRelay::start();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("kanban.db")).unwrap();
    let client = Client::new(&relay.url, &relay.key("volker", "v1"), Duration::from_secs(5));
    assert_eq!(store.origin_tag().unwrap(), None);
    check_identity(&store, &client).unwrap();
    assert_eq!(store.origin_tag().unwrap().as_deref(), Some("v1"));
    assert_eq!(store.paused().unwrap(), None);
}

#[test]
fn a_fresh_store_on_a_key_already_in_use_elsewhere_is_still_caught() {
    // The same key on a second machine: the fresh store takes the tag, and the high-water check
    // right after it stops it before it reuses the first machine's numbers.
    let relay = HttpRelay::start();
    let first = machine(&relay, "volker", "v1", "x");
    first.store.create_task(first.pid, TaskDraft::new("published from the first machine")).unwrap();
    sync_board(&first.store, &first.client, first.pid).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let second = Store::open(&dir.path().join("kanban.db")).unwrap();
    let err = check_identity(&second, &first.client).unwrap_err();
    assert!(matches!(err, RemoteError::Paused(ref why) if why.contains("in use on another machine")), "{err:?}");
}

#[test]
fn a_store_restored_from_a_backup_is_caught_before_it_reuses_ids() {
    let relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    benny.store.create_task(benny.pid, TaskDraft::new("before the backup")).unwrap();
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();
    let backup = benny._dir.path().join("backup.db");
    benny.store.backup_to(&backup).unwrap();

    benny.store.create_task(benny.pid, TaskDraft::new("after the backup")).unwrap();
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();

    let restored = Store::open(&backup).unwrap();
    let pid = restored.board_of_room("sbahn").unwrap().unwrap();
    let err = check_identity(&restored, &benny.client).unwrap_err();
    assert!(matches!(err, RemoteError::Paused(ref why) if why.contains("restored from a backup")), "{err:?}");
    assert!(matches!(sync_board(&restored, &benny.client, pid), Err(RemoteError::Paused(_))));
    // And the relay is untouched: one "after the backup" task, not two.
    let carla = machine(&relay, "carla", "c1", "c");
    sync_board(&carla.store, &carla.client, carla.pid).unwrap();
    assert_eq!(titles(&carla), ["after the backup", "before the backup"]);
}

#[test]
fn a_relay_that_speaks_another_protocol_pauses_sync() {
    use axum::routing::get;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            let app = axum::Router::new().route("/v1/hello", get(|| async {
                axum::Json(serde_json::json!({ "protocol_min": 7, "protocol_max": 9, "server_version": "9.0.0" }))
            }));
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app).await.unwrap();
        });
    });
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let err = check_identity(&s, &Client::new(&format!("http://{addr}"), "akb_x", Duration::from_secs(5))).unwrap_err();
    assert!(matches!(err, RemoteError::Paused(ref why) if why.contains("protocol 7-9")), "{err:?}");
}

#[test]
fn two_sessions_syncing_one_store_at_once_agree() {
    let relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    for i in 0..30 {
        benny.store.create_task(benny.pid, TaskDraft::new(format!("task {i}"))).unwrap();
    }
    // A second agent session on the same machine: another connection to the same file.
    let second = Store::open(&benny.path).unwrap();
    let (c1, c2) = (benny.client.clone(), benny.client.clone());
    let pid = benny.pid;
    let p2 = benny.path.clone();
    let a = std::thread::spawn(move || {
        let s = Store::open(&p2).unwrap();
        for _ in 0..5 { let _ = sync_board(&s, &c1, pid); }
    });
    for _ in 0..5 { let _ = sync_board(&second, &c2, pid); }
    a.join().unwrap();
    sync_board(&benny.store, &benny.client, pid).unwrap();

    let volker = machine(&relay, "volker", "v1", "x");
    sync_board(&volker.store, &volker.client, volker.pid).unwrap();
    assert_eq!(titles(&volker).len(), 30, "each task once: {:?}", titles(&volker));
    assert_eq!(benny.store.sync_status(pid).unwrap().unwrap().unsent, 0);
}

#[test]
fn the_notification_stream_announces_a_teammates_push() {
    let relay = HttpRelay::start();
    let benny = machine(&relay, "benny", "b1", "sbb");
    let volker = machine(&relay, "volker", "v1", "x");
    let watcher = volker.client.clone();
    let seen = std::thread::spawn(move || {
        let mut heads = vec![];
        let _ = watcher.watch("sbahn", |h| { heads.extend(h); heads.len() < 2 });
        heads
    });
    std::thread::sleep(Duration::from_millis(300));
    benny.store.create_task(benny.pid, TaskDraft::new("news")).unwrap();
    sync_board(&benny.store, &benny.client, benny.pid).unwrap();
    let heads = seen.join().unwrap();
    assert_eq!(heads.len(), 2, "{heads:?}");
    assert!(heads[1] > heads[0], "{heads:?}");
}

/// The realistic bad network: a relay address that accepts the connection and then never says a
/// word (a stalled proxy, captive wifi). The round before a claim must still end quickly.
#[test]
fn the_round_before_a_claim_is_bounded_even_by_a_relay_that_never_answers() {
    use ai_kanban::remote::sync_board_within;
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = silent.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for c in silent.incoming().flatten() {
            held.push(c);
        }
    });
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("sbb").unwrap().id;
    s.connect_board(pid, "sbahn").unwrap();
    s.create_task(pid, TaskDraft::new("claimed while the network is bad")).unwrap();

    let client = Client::new(&format!("http://{addr}"), "akb_x", Duration::from_millis(1200));
    let start = std::time::Instant::now();
    let r = sync_board_within(&s, &client, pid, Duration::from_millis(2500));
    assert!(start.elapsed() < Duration::from_millis(3500), "took {:?}", start.elapsed());
    assert!(matches!(r, Err(RemoteError::Offline(_))), "{r:?}");
    assert!(s.sync_status(pid).unwrap().unwrap().last_error.unwrap().starts_with("offline:"));
    assert_eq!(titles_of(&s, pid), ["claimed while the network is bad"], "local work is untouched");
}

fn titles_of(s: &Store, pid: i64) -> Vec<String> {
    s.board(pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect()
}
