//! The client half of sync, end to end against the real relay in-process: members' stores
//! flush into the relay's log and apply what they pull from it. No network.
//!
//! The test this exists for is `a_board_that_is_not_connected_publishes_nothing`: everything
//! else is about sharing correctly, that one is about never sharing what was not meant to be.

use ai_kanban::core::model::*;
use ai_kanban::core::note::{NoteDraft, NotePatch};
use ai_kanban::core::sync::{published_kind, EVENT_FIELDS, EVENT_LOCAL, NOTE_FIELDS, NOTE_LOCAL, TASK_FIELDS, TASK_LOCAL};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::{Error, Store};
use ai_kanban::sync::protocol::{Change, Clock, Pulled, Ref};
use serde_json::json;
use std::collections::BTreeMap;

mod common;
use common::sync::*;

/// What a board holds, by identity rather than local id, so two stores can be compared.
fn state(s: &Store, pid: i64) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let ident = |kind: &str, col: &str| format!(
        "(SELECT origin || ':' || origin_id FROM sync_ids WHERE kind = '{kind}' AND local_id = {col})"
    );
    let q = format!(
        "SELECT {}, title, body, status, type, priority, tags, COALESCE(external_ref, ''),
                COALESCE((SELECT name FROM workstreams w WHERE w.id = t.workstream_id), ''), COALESCE({}, '')
           FROM tasks t WHERE project_id = ?1",
        ident("task", "t.id"), ident("task", "t.blocked_by")
    );
    let mut st = s.conn.prepare(&q).unwrap();
    for row in st.query_map([pid], |r| Ok((0..10).map(|i| r.get::<_, String>(i).unwrap_or_default()).collect::<Vec<_>>())).unwrap() {
        let row = row.unwrap();
        out.insert(format!("task {}", row[0]), row[1..].join(" | "));
    }
    let q = format!(
        "SELECT {}, title, body, tags, COALESCE({}, ''),
                COALESCE((SELECT group_concat(path, ',') FROM (SELECT path FROM note_paths WHERE note_id = n.id ORDER BY path)), '')
           FROM notes n WHERE project_id = ?1",
        ident("note", "n.id"), ident("task", "n.task_id")
    );
    let mut st = s.conn.prepare(&q).unwrap();
    for row in st.query_map([pid], |r| Ok((0..6).map(|i| r.get::<_, String>(i).unwrap_or_default()).collect::<Vec<_>>())).unwrap() {
        let row = row.unwrap();
        out.insert(format!("note {}", row[0]), row[1..].join(" | "));
    }
    // Only what a room carries: each machine also keeps events of its own.
    let q = format!(
        "SELECT {}, kind, body, COALESCE({}, ''), COALESCE({}, '') FROM events e WHERE project_id = ?1",
        ident("event", "e.id"), ident("task", "e.task_id"), ident("note", "e.note_id")
    );
    let mut st = s.conn.prepare(&q).unwrap();
    for row in st.query_map([pid], |r| Ok((0..5).map(|i| r.get::<_, String>(i).unwrap_or_default()).collect::<Vec<_>>())).unwrap() {
        let row = row.unwrap();
        if published_kind(&row[1]) {
            out.insert(format!("event {}", row[0]), row[1..].join(" | "));
        }
    }
    out
}

/// (id, body) of every note on a board.
fn notes(s: &Store, pid: i64) -> Vec<(i64, String)> {
    let mut st = s.conn.prepare("SELECT id, body FROM notes WHERE project_id = ?1 ORDER BY id").unwrap();
    st.query_map([pid], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
}

fn patch() -> TaskPatch {
    TaskPatch { actor: Actor::Agent, ..Default::default() }
}

fn count(s: &Store, table: &str) -> i64 {
    s.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

// ---------------------------------------------------------------------------
// What is synced at all
// ---------------------------------------------------------------------------

/// Every column is either synced or deliberately local. A new column in neither list fails
/// here instead of silently never reaching teammates.
#[test]
fn every_column_is_classified_as_synced_or_local() {
    let s = Store::open_in_memory().unwrap();
    for (table, synced, local) in [
        ("tasks", TASK_FIELDS, TASK_LOCAL),
        ("notes", NOTE_FIELDS, NOTE_LOCAL),
        ("events", EVENT_FIELDS, EVENT_LOCAL),
    ] {
        let mut st = s.conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')")).unwrap();
        let cols: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(|c| c.unwrap()).collect();
        for col in &cols {
            let known = local.contains(&col.as_str()) || synced.iter().any(|f| f.column == col);
            assert!(known, "{table}.{col} is neither synced nor listed as local");
        }
        for f in synced.iter().filter(|f| !f.column.is_empty()) {
            assert!(cols.iter().any(|c| c == f.column), "{table}.{} is synced but does not exist", f.column);
        }
    }
}

#[test]
fn a_board_that_is_not_connected_publishes_nothing() {
    let relay = relay();
    let m = member(&relay, "benny", "b1", "team");
    m.store.flush().unwrap();
    m.store.outbox_sent(m.pid, i64::MAX).unwrap();
    let private = m.store.create_board("customer-x").unwrap().id;

    let t = m.store.create_task(private, TaskDraft::new("secret customer work")).unwrap();
    m.store.update_task(private, t.id, TaskPatch { status: Some(Status::Doing), log: Some("why".into()), ..patch() }).unwrap();
    m.store.create_note(private, NoteDraft { task_id: Some(t.id), paths: vec!["a.rs".into()], ..NoteDraft::new("secret note") }, Actor::Agent).unwrap();
    m.store.log(private, Actor::Agent, "secret decision").unwrap();
    m.store.forget_task(private, t.id).unwrap();

    assert_eq!(m.store.flush().unwrap(), 0);
    assert_eq!(count(&m.store, "sync_outbox"), 0);
    let ids: i64 = m.store.conn.query_row(
        "SELECT COUNT(*) FROM sync_ids s JOIN tasks t ON s.kind = 'task' AND t.id = s.local_id WHERE t.project_id = ?1",
        [private], |r| r.get(0)).unwrap();
    assert_eq!(ids, 0, "rows on a private board never get a sync identity");
}

#[test]
fn connecting_needs_an_origin_tag_and_one_room_per_board_and_board_per_room() {
    let s = Store::open_in_memory().unwrap();
    let a = s.create_board("a").unwrap().id;
    let b = s.create_board("b").unwrap().id;
    assert!(s.connect_board(a, ROOM).unwrap_err().to_string().contains("origin tag"));
    assert!(s.set_origin_tag("b-1").is_err());
    assert!(s.set_origin_tag("srv").is_err(), "the relay's own tag");
    s.set_origin_tag("b1").unwrap();
    assert!(s.connect_board(a, "no spaces").is_err());
    s.connect_board(a, "Sbahn").unwrap();
    assert_eq!(s.room_of(a).unwrap().as_deref(), Some("sbahn"), "one spelling");
    assert!(s.connect_board(a, "other").unwrap_err().to_string().contains("already connected"));
    assert!(s.connect_board(b, "sbahn").unwrap_err().to_string().contains("already connected to board a"));
    assert!(s.set_origin_tag("b2").is_err(), "the tag cannot change under a connected board");
    s.set_origin_tag("b1").unwrap();
}

// ---------------------------------------------------------------------------
// Sharing
// ---------------------------------------------------------------------------

#[test]
fn connecting_publishes_what_the_board_already_holds() {
    let relay = relay();
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("sbb").unwrap().id;
    let t = s.create_task(pid, TaskDraft { body: "details".into(), tags: vec!["infra".into()], ..TaskDraft::new("existing task") }).unwrap();
    s.create_note(pid, NoteDraft { task_id: Some(t.id), paths: vec!["src/a.rs".into()], ..NoteDraft::new("existing note") }, Actor::Agent).unwrap();
    s.log(pid, Actor::Agent, "an old decision").unwrap();
    s.connect_board(pid, ROOM).unwrap();
    let key = relay.add_key("benny", "b1", "").unwrap();
    let benny = Member { store: s, pid, p: relay.authenticate(&key).unwrap() };
    relay.join(ROOM, &benny.p).unwrap();
    sync(&relay, &benny);

    let volker = member(&relay, "volker", "v1", "sbahn upgrade");
    sync(&relay, &volker);
    let got = task_by_title(&volker, "existing task");
    assert_eq!(got.body, "details");
    assert_eq!(volker.store.task_tags(volker.pid, got.id).unwrap(), ["infra"]);
    let detail = volker.store.task_detail(volker.pid, got.id).unwrap();
    assert_eq!(detail.notes.len(), 1, "the note arrived attached to its task");
    assert_eq!(detail.notes[0].title, "existing note");
    assert!(detail.events.iter().any(|e| e.kind == "created"), "its history came along");
    assert!(volker.store.recent_events(volker.pid, 20).unwrap().iter().any(|e| e.body == "an old decision"));
    assert_eq!(state(&benny.store, benny.pid), state(&volker.store, volker.pid));
}

#[test]
fn edits_and_history_travel_both_ways_including_on_your_own_tasks() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "sbahn upgrade");
    let mine = benny.store.create_task(benny.pid, TaskDraft::new("auth timeout")).unwrap();
    converge(&relay, &[&benny, &volker]);

    // Volker finds the root cause on Benny's task.
    let theirs = task_by_title(&volker, "auth timeout");
    volker.store.update_task(volker.pid, theirs.id, TaskPatch { status: Some(Status::Doing), log: Some("taking it".into()), ..patch() }).unwrap();
    volker.store.log_on(volker.pid, Some(theirs.id), Actor::Agent, "root cause: the proxy drops keep-alive").unwrap();
    volker.store.create_note(volker.pid, NoteDraft { task_id: Some(theirs.id), ..NoteDraft::new("proxy keep-alive") }, Actor::Agent).unwrap();
    converge(&relay, &[&benny, &volker]);

    let back = benny.store.task_detail(benny.pid, mine.id).unwrap();
    assert_eq!(back.task.status, Status::Doing);
    assert!(back.events.iter().any(|e| e.body.contains("root cause")), "a teammate's finding on my task reached me");
    assert_eq!(back.notes.len(), 1);
    assert_eq!(state(&benny.store, benny.pid), state(&volker.store, volker.pid));
}

#[test]
fn an_edit_publishes_only_the_fields_it_changed() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("t")).unwrap();
    push(&relay, &benny);
    benny.store.update_task(benny.pid, t.id, TaskPatch { priority: Some(Priority::High), ..patch() }).unwrap();
    benny.store.flush().unwrap();
    let fields: Vec<Option<String>> = benny.store.outbox(benny.pid, 100).unwrap().into_iter()
        .filter(|(_, c)| c.entity.kind == "task").map(|(_, c)| c.field).collect();
    assert_eq!(fields, [Some("priority".into())], "the status_changed-style event goes separately; the task sends one field");
}

#[test]
fn a_settled_room_stops_talking() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    benny.store.create_task(benny.pid, TaskDraft::new("one")).unwrap();
    converge(&relay, &[&benny, &volker]);
    assert_eq!(benny.store.flush().unwrap(), 0, "applying a teammate's changes is not echoed back");
    assert_eq!(volker.store.flush().unwrap(), 0);
    let head = relay.pull(ROOM, &benny.p, 0, 1000).unwrap().head;
    converge(&relay, &[&benny, &volker]);
    assert_eq!(relay.pull(ROOM, &benny.p, 0, 1000).unwrap().head, head, "nothing new was pushed");
}

#[test]
fn applying_the_same_changes_twice_changes_nothing() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    benny.store.create_task(benny.pid, TaskDraft::new("one")).unwrap();
    push(&relay, &benny);
    let page = relay.pull(ROOM, &volker.p, 0, 1000).unwrap();
    volker.store.apply(volker.pid, &page.changes).unwrap();
    let before = state(&volker.store, volker.pid);
    let again = volker.store.apply(volker.pid, &page.changes).unwrap();
    assert_eq!(again.applied, 0, "{again:?}");
    assert_eq!(state(&volker.store, volker.pid), before);
}

#[test]
fn concurrent_edits_of_one_field_settle_on_the_same_winner_and_other_fields_both_survive() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("shared")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "shared");

    // Both offline: same field, and one different field each.
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), priority: Some(Priority::High), ..patch() }).unwrap();
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Blocked), body: Some("needs a cert".into()), ..patch() }).unwrap();
    push(&relay, &volker);
    push(&relay, &benny);
    converge(&relay, &[&benny, &volker]);

    let b = benny.store.task(benny.pid, t.id).unwrap();
    let v = volker.store.task(volker.pid, vt.id).unwrap();
    assert_eq!(b.status, v.status, "one winner everywhere");
    assert_eq!((b.priority, b.body.as_str()), (Priority::High, "needs a cert"), "different fields both survive");
    assert_eq!(state(&benny.store, benny.pid), state(&volker.store, volker.pid));
}

#[test]
fn the_newest_edit_wins_not_the_last_one_to_sync() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("shared")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "shared");

    // Both offline. Benny claims it; a moment later volker finishes it. Volker comes online
    // first, benny only afterwards -- an edit from the morning must not reopen it in the evening.
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Done), ..patch() }).unwrap();
    push(&relay, &volker);
    std::thread::sleep(std::time::Duration::from_millis(20));
    push(&relay, &benny);
    converge(&relay, &[&benny, &volker]);

    assert_eq!(benny.store.task(benny.pid, t.id).unwrap().status, Status::Done);
    assert_eq!(volker.store.task(volker.pid, vt.id).unwrap().status, Status::Done);
}

#[test]
fn a_teammate_whose_clock_runs_ahead_does_not_win_my_later_edit() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("t")).unwrap();
    push(&relay, &benny);
    let me = benny.store.identity("task", t.id).unwrap().unwrap();

    // A change stamped an hour in the future, from a machine with a fast clock.
    let future = ai_kanban::core::now() * 1000 + 3_600_000;
    let ahead = Pulled {
        seq: 999, author: "volker".into(), received_at: 0,
        change: Change {
            cid: "v1:1".into(), entity: me.clone(), subject: Some(me.clone()), field: Some("title".into()),
            value: json!("their title"), clock: Clock { wall_ms: future, counter: 0, tag: "v1".into() }, extra: Default::default(),
        },
    };
    benny.store.apply(benny.pid, &[ahead]).unwrap();
    benny.store.update_task(benny.pid, t.id, TaskPatch { title: Some("my later title".into()), ..patch() }).unwrap();
    benny.store.flush().unwrap();
    let mine = benny.store.outbox(benny.pid, 100).unwrap().into_iter().find(|(_, c)| c.field.as_deref() == Some("title")).unwrap().1;
    assert!(mine.clock.wall_ms >= future, "my clock moved past the one I saw, so my edit wins");
}

// ---------------------------------------------------------------------------
// Order does not matter
// ---------------------------------------------------------------------------

/// A small seeded generator, so a failing order can be reproduced from its seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, (self.next() % (i as u64 + 1)) as usize);
        }
    }
}

#[test]
fn any_order_of_the_same_changes_gives_the_same_board() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");

    let a = benny.store.create_task(benny.pid, TaskDraft { tags: vec!["x".into()], ..TaskDraft::new("blocker") }).unwrap();
    let ws = benny.store.ensure_workstream(benny.pid, "deploy").unwrap();
    let b = benny.store.create_task(benny.pid, TaskDraft { blocked_by: Some(a.id), ..TaskDraft::new("blocked") }).unwrap();
    benny.store.update_task(benny.pid, b.id, TaskPatch { workstream: Some(Some(ws.id)), ..patch() }).unwrap();
    benny.store.create_note(benny.pid, NoteDraft { task_id: Some(a.id), paths: vec!["z.rs".into(), "a.rs".into()], ..NoteDraft::new("n") }, Actor::Agent).unwrap();
    converge(&relay, &[&benny, &volker]);
    let va = task_by_title(&volker, "blocker");
    volker.store.update_task(volker.pid, va.id, TaskPatch { status: Some(Status::Done), log: Some("fixed".into()), ..patch() }).unwrap();
    benny.store.update_task(benny.pid, a.id, TaskPatch { title: Some("blocker, renamed".into()), ..patch() }).unwrap();
    volker.store.log(volker.pid, Actor::Agent, "board-level decision").unwrap();
    converge(&relay, &[&benny, &volker]);
    let expected = state(&benny.store, benny.pid);
    assert_eq!(state(&volker.store, volker.pid), expected);

    let log = relay.pull(ROOM, &benny.p, 0, 1000).unwrap().changes;
    let key = relay.add_key("carla", "c1", "").unwrap();
    let carla_p = relay.authenticate(&key).unwrap();
    for seed in 1..=25u64 {
        let mut order = log.clone();
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)).shuffle(&mut order);
        let s = Store::open_in_memory().unwrap();
        s.set_origin_tag("c1").unwrap();
        let pid = s.create_board("carla").unwrap().id;
        s.connect_board(pid, ROOM).unwrap();
        // One change per apply, the harshest version of "in any order".
        for p in &order {
            s.apply(pid, std::slice::from_ref(p)).unwrap();
        }
        let _ = &carla_p;
        assert_eq!(state(&s, pid), expected, "seed {seed}");
        assert_eq!(s.flush().unwrap(), 0, "seed {seed}: a cold member publishes nothing it only received");
    }
}

#[test]
fn an_event_that_arrives_before_its_task_is_kept_and_the_task_fills_in_later() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("late title")).unwrap();
    benny.store.log_on(benny.pid, Some(t.id), Actor::Agent, "early event").unwrap();
    push(&relay, &benny);

    let all = relay.pull(ROOM, &volker.p, 0, 1000).unwrap().changes;
    let (events, fields): (Vec<_>, Vec<_>) = all.into_iter().partition(|p| p.change.entity.kind == "event");
    volker.store.apply(volker.pid, &events).unwrap();
    let ghost = volker.store.board(volker.pid, &BoardQuery::board()).unwrap().tasks;
    assert_eq!(ghost.len(), 1, "a minimal task holds the event");
    volker.store.apply(volker.pid, &fields).unwrap();
    let detail = volker.store.task_detail(volker.pid, ghost[0].id).unwrap();
    assert_eq!(detail.task.title, "late title");
    assert!(detail.events.iter().any(|e| e.body == "early event"));
}

// ---------------------------------------------------------------------------
// Forgetting, leaving, parking
// ---------------------------------------------------------------------------

#[test]
fn a_room_forget_removes_the_task_from_every_member_and_keeps_its_notes() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("leaked AKIAsecret")).unwrap();
    benny.store.create_note(benny.pid, NoteDraft { task_id: Some(t.id), ..NoteDraft::new("learned") }, Actor::Agent).unwrap();
    converge(&relay, &[&benny, &volker]);

    relay.forget(ROOM, &benny.store.identity("task", t.id).unwrap().unwrap()).unwrap();
    converge(&relay, &[&benny, &volker]);
    for m in [&benny, &volker] {
        let titles: Vec<String> = m.store.board(m.pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
        assert!(titles.is_empty(), "{}: {titles:?}", m.p.tag);
        assert_eq!(notes(&m.store, m.pid).len(), 1, "{}: the note survives, detached", m.p.tag);
    }
}

#[test]
fn a_change_that_arrives_after_a_tombstone_cannot_bring_the_task_back() {
    let relay = relay();
    let volker = member(&relay, "volker", "v1", "x");
    let gone = Ref { kind: "task".into(), origin: "b1".into(), id: 812 };
    let pulled = |seq, cid: &str, field: &str, value, tag: &str| Pulled {
        seq, author: "x".into(), received_at: 0,
        change: Change {
            cid: cid.into(), entity: gone.clone(), subject: if tag == "srv" { None } else { Some(gone.clone()) },
            field: Some(field.into()), value, clock: Clock { wall_ms: seq * 10, counter: 0, tag: tag.into() }, extra: Default::default(),
        },
    };
    volker.store.apply(volker.pid, &[pulled(1, "srv:1", "forgotten", json!(true), "srv")]).unwrap();
    let r = volker.store.apply(volker.pid, &[pulled(2, "b1:5", "title", json!("secret"), "b1")]).unwrap();
    assert_eq!(r.ignored, 1);
    assert!(volker.store.board(volker.pid, &BoardQuery::board()).unwrap().tasks.is_empty());
}

#[test]
fn moving_a_task_onto_a_connected_board_publishes_it_with_its_history_but_not_the_board_it_came_from() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let private = benny.store.create_board("customer-secret-name").unwrap().id;
    let t = benny.store.create_task(private, TaskDraft::new("now shared")).unwrap();
    benny.store.log_on(private, Some(t.id), Actor::Agent, "history before the move").unwrap();
    benny.store.move_task(private, t.id, benny.pid, Actor::Agent, Some("team needs it"), None).unwrap();
    converge(&relay, &[&benny, &volker]);

    let got = task_by_title(&volker, "now shared");
    let detail = volker.store.task_detail(volker.pid, got.id).unwrap();
    assert!(detail.events.iter().any(|e| e.body == "history before the move"));
    let everything = serde_json::to_string(&relay.pull(ROOM, &benny.p, 0, 1000).unwrap()).unwrap();
    assert!(!everything.contains("customer-secret-name"), "a private board's name never reaches the room");
}

#[test]
fn moving_a_task_off_leaves_teammates_a_frozen_archived_copy() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let private = benny.store.create_board("mine").unwrap().id;
    let t = benny.store.create_task(benny.pid, TaskDraft::new("taken private")).unwrap();
    converge(&relay, &[&benny, &volker]);
    benny.store.move_task(benny.pid, t.id, private, Actor::Agent, None, None).unwrap();
    converge(&relay, &[&benny, &volker]);

    let copy = task_by_title(&volker, "taken private");
    assert_eq!(copy.status, Status::Archived);
    let detail = volker.store.task_detail(volker.pid, copy.id).unwrap();
    assert!(detail.events.iter().any(|e| e.kind == "left_room" && e.body.contains("benny")), "{:?}", detail.events);
    assert_eq!(benny.store.task(private, t.id).unwrap().status, Status::Backlog, "the mover keeps it as it was");

    // Edits on the private board after the move stay private.
    benny.store.update_task(private, t.id, TaskPatch { title: Some("private now".into()), ..patch() }).unwrap();
    assert_eq!(benny.store.flush().unwrap(), 0);
}

#[test]
fn a_change_this_binary_cannot_store_is_parked_and_the_rest_still_apply() {
    let relay = relay();
    let volker = member(&relay, "volker", "v1", "x");
    let t = Ref { kind: "task".into(), origin: "b1".into(), id: 1 };
    let change = |seq: i64, field: &str, value| Pulled {
        seq, author: "benny".into(), received_at: 0,
        change: Change {
            cid: format!("b1:{seq}"), entity: t.clone(), subject: Some(t.clone()), field: Some(field.into()), value,
            clock: Clock { wall_ms: 100 + seq, counter: 0, tag: "b1".into() }, extra: Default::default(),
        },
    };
    let r = volker.store.apply(volker.pid, &[
        change(1, "title", json!("arrives fine")),
        change(2, "status", json!("in-review")),     // a status a newer client knows
        change(3, "colour", json!("red")),           // a field a newer client added
        change(4, "priority", json!("high")),
    ]).unwrap();
    assert_eq!((r.applied, r.parked), (2, 2), "{r:?}");
    let task = volker.store.board(volker.pid, &BoardQuery::board()).unwrap().tasks.remove(0);
    assert_eq!((task.title.as_str(), task.priority, task.status), ("arrives fine", Priority::High, Status::Backlog));
    assert_eq!(volker.store.sync_status(volker.pid).unwrap().unwrap().parked, 2);

    // Still unknown after "re-applying": kept, not lost.
    let again = volker.store.reapply_parked(volker.pid).unwrap();
    assert_eq!(again.parked, 2);
    assert_eq!(volker.store.sync_status(volker.pid).unwrap().unwrap().parked, 2);
}

#[test]
fn a_parked_change_reapplied_later_still_loses_to_a_newer_value() {
    let relay = relay();
    let volker = member(&relay, "volker", "v1", "x");
    let t = Ref { kind: "task".into(), origin: "b1".into(), id: 1 };
    let change = |seq: i64, wall: i64, title: &str| Pulled {
        seq, author: "benny".into(), received_at: 0,
        change: Change {
            cid: format!("b1:{seq}"), entity: t.clone(), subject: Some(t.clone()), field: Some("title".into()), value: json!(title),
            clock: Clock { wall_ms: wall, counter: 0, tag: "b1".into() }, extra: Default::default(),
        },
    };
    volker.store.apply(volker.pid, &[change(2, 200, "newer")]).unwrap();
    // Parked earlier by an older binary, now applicable.
    let parked = serde_json::to_string(&change(1, 100, "older")).unwrap();
    volker.store.conn.execute("INSERT INTO sync_parked (project_id, body, reason, at) VALUES (?1, ?2, 'old binary', 0)", rusqlite::params![volker.pid, parked]).unwrap();
    let r = volker.store.reapply_parked(volker.pid).unwrap();
    assert_eq!((r.ignored, r.parked), (1, 0));
    assert_eq!(volker.store.board(volker.pid, &BoardQuery::board()).unwrap().tasks[0].title, "newer");
}

#[test]
fn housekeeping_never_reaches_the_room() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    benny.store.write_event(benny.pid, None, Actor::System, "path_learned", "/Users/benny/private/path").unwrap();
    benny.store.write_event(benny.pid, None, Actor::System, "workstream_entered", "deploy").unwrap();
    push(&relay, &benny);
    let log = serde_json::to_string(&relay.pull(ROOM, &benny.p, 0, 1000).unwrap()).unwrap();
    assert!(!log.contains("/Users/benny"), "{log}");
    assert!(!log.contains("workstream_entered"));
}

#[test]
fn workstreams_created_on_two_machines_merge_by_name() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    for (m, title) in [(&benny, "b's"), (&volker, "v's")] {
        let ws = m.store.ensure_workstream(m.pid, "deploy").unwrap();
        let t = m.store.create_task(m.pid, TaskDraft::new(title)).unwrap();
        m.store.update_task(m.pid, t.id, TaskPatch { workstream: Some(Some(ws.id)), ..patch() }).unwrap();
    }
    converge(&relay, &[&benny, &volker]);
    for m in [&benny, &volker] {
        let n: i64 = m.store.conn.query_row("SELECT COUNT(*) FROM workstreams WHERE project_id = ?1", [m.pid], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "{}", m.p.tag);
    }
    assert_eq!(state(&benny.store, benny.pid), state(&volker.store, volker.pid));
}

#[test]
fn note_edits_paths_and_their_task_link_sync() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("t")).unwrap();
    let n = benny.store.create_note(benny.pid, NoteDraft { task_id: Some(t.id), paths: vec!["a.rs".into()], ..NoteDraft::new("n") }, Actor::Agent).unwrap();
    converge(&relay, &[&benny, &volker]);
    benny.store.update_note(benny.pid, n.id, NotePatch { body: Some("better".into()), paths: Some(vec!["b.rs".into(), "a.rs".into()]), actor: Actor::Agent, ..Default::default() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let (vid, body) = notes(&volker.store, volker.pid).remove(0);
    assert_eq!(body, "better");
    let mut st = volker.store.conn.prepare("SELECT path FROM note_paths WHERE note_id = ?1 ORDER BY path").unwrap();
    let paths: Vec<String> = st.query_map([vid], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(paths, ["a.rs", "b.rs"]);
    assert_eq!(state(&benny.store, benny.pid), state(&volker.store, volker.pid));
}

// ---------------------------------------------------------------------------
// Refusals on a connected board
// ---------------------------------------------------------------------------

#[test]
fn a_connected_board_refuses_local_forgets_and_merges() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("t")).unwrap();
    let n = benny.store.create_note(benny.pid, NoteDraft::new("n"), Actor::Agent).unwrap();
    let other = benny.store.create_board("other").unwrap().id;
    let shared = |r: Result<(), Error>| matches!(r, Err(Error::SharedBoard { .. }));
    assert!(shared(benny.store.forget_task(benny.pid, t.id)));
    assert!(shared(benny.store.forget_note(benny.pid, n.id)));
    assert!(shared(benny.store.forget_board(benny.pid)));
    assert!(matches!(benny.store.merge_projects(other, benny.pid), Err(Error::SharedBoard { .. })));
    assert!(matches!(benny.store.merge_projects(benny.pid, other), Err(Error::SharedBoard { .. })));
    let msg = benny.store.forget_task(benny.pid, t.id).unwrap_err().to_string();
    assert!(msg.contains("ai-kanban-relay forget sbahn task"), "the refusal says who can: {msg}");
    assert!(benny.store.task(benny.pid, t.id).is_ok());
}

#[tokio::test]
async fn the_web_api_refuses_to_forget_on_a_connected_board_with_409() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("sbb").unwrap().id;
    let t = s.create_task(pid, TaskDraft::new("t")).unwrap();
    s.connect_board(pid, ROOM).unwrap();
    let (tx, _) = tokio::sync::broadcast::channel(4);
    let api = ai_kanban::http::Api { store: std::sync::Arc::new(std::sync::Mutex::new(s)), changes: tx };
    let req = Request::builder().method("DELETE").uri(format!("/api/projects/{pid}/tasks/{}", t.id)).body(Body::empty()).unwrap();
    let res = ai_kanban::http::router(api.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "shared");
    assert_eq!(body["error"]["room"], "sbahn");
}

// ---------------------------------------------------------------------------
// Disconnecting
// ---------------------------------------------------------------------------

#[test]
fn disconnecting_keeps_or_wipes_foreign_rows_and_reconnecting_does_not_duplicate() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    benny.store.create_task(benny.pid, TaskDraft::new("benny's")).unwrap();
    volker.store.create_task(volker.pid, TaskDraft::new("volker's")).unwrap();
    converge(&relay, &[&benny, &volker]);
    benny.store.create_task(benny.pid, TaskDraft::new("unsent")).unwrap();
    benny.store.flush().unwrap();

    benny.store.disconnect_board(benny.pid, false).unwrap();
    assert_eq!(benny.store.room_of(benny.pid).unwrap(), None);
    assert_eq!(count(&benny.store, "sync_outbox"), 0, "unsent changes are dropped; the CLI asks first");
    task_by_title(&benny, "volker's");
    benny.store.create_task(benny.pid, TaskDraft::new("while disconnected")).unwrap();
    assert_eq!(benny.store.flush().unwrap(), 0, "a disconnected board is private again");

    benny.store.connect_board(benny.pid, ROOM).unwrap();
    converge(&relay, &[&benny, &volker]);
    let titles: Vec<String> = benny.store.board(benny.pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert_eq!(titles.iter().filter(|t| *t == "volker's").count(), 1, "matched again, not duplicated: {titles:?}");
    assert_eq!(state(&benny.store, benny.pid), state(&volker.store, volker.pid));

    volker.store.disconnect_board(volker.pid, true).unwrap();
    let left: Vec<String> = volker.store.board(volker.pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert_eq!(left, ["volker's"], "only the rows other members created are wiped");
}

#[test]
fn wiping_on_disconnect_takes_only_tasks_others_created_and_keeps_their_findings_on_mine() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let mine = benny.store.create_task(benny.pid, TaskDraft::new("mine")).unwrap();
    volker.store.create_task(volker.pid, TaskDraft::new("theirs")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let on_mine = task_by_title(&volker, "mine");
    volker.store.log_on(volker.pid, Some(on_mine.id), Actor::Agent, "their finding on my task").unwrap();
    volker.store.create_note(volker.pid, NoteDraft { task_id: Some(on_mine.id), ..NoteDraft::new("their note on my task") }, Actor::Agent).unwrap();
    volker.store.log(volker.pid, Actor::Agent, "their board-level decision").unwrap();
    converge(&relay, &[&benny, &volker]);

    benny.store.disconnect_board(benny.pid, true).unwrap();
    let titles: Vec<String> = benny.store.board(benny.pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert_eq!(titles, ["mine"]);
    let detail = benny.store.task_detail(benny.pid, mine.id).unwrap();
    assert!(detail.events.iter().any(|e| e.body == "their finding on my task"), "{:?}", detail.events);
    assert_eq!(detail.notes.len(), 1, "their note on my task stays");
    assert!(benny.store.recent_events(benny.pid, 50).unwrap().iter().any(|e| e.body == "their board-level decision"));
}

/// A store restored from a backup takes a new tag. Its work from before is still its own.
#[test]
fn work_published_under_an_earlier_tag_stays_this_stores_own() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let old = benny.store.create_task(benny.pid, TaskDraft::new("from before the restore")).unwrap();
    volker.store.create_task(volker.pid, TaskDraft::new("theirs")).unwrap();
    converge(&relay, &[&benny, &volker]);

    benny.store.disconnect_board(benny.pid, false).unwrap();
    benny.store.set_origin_tag("b2").unwrap();
    assert_eq!(benny.store.resolve_task_arg(benny.pid, &old.id.to_string()).unwrap(), old.id, "its plain number still resolves");
    benny.store.connect_board(benny.pid, ROOM).unwrap();
    benny.store.disconnect_board(benny.pid, true).unwrap();
    let titles: Vec<String> = benny.store.board(benny.pid, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert_eq!(titles, ["from before the restore"], "the wipe took only volker's task");
}

#[test]
fn a_teammates_event_about_a_task_i_took_private_does_not_link_the_boards() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let private = benny.store.create_board("private").unwrap().id;
    volker.store.create_task(volker.pid, TaskDraft::new("theirs")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let here = task_by_title(&benny, "theirs");
    benny.store.move_task(benny.pid, here.id, private, Actor::Agent, None, None).unwrap();
    // Volker, who has not synced yet, keeps working on it.
    let there = task_by_title(&volker, "theirs");
    volker.store.log_on(volker.pid, Some(there.id), Actor::Agent, "still on it").unwrap();
    let blocked = volker.store.create_task(volker.pid, TaskDraft { blocked_by: Some(there.id), ..TaskDraft::new("waits on theirs") }).unwrap();
    let _ = blocked;
    converge(&relay, &[&benny, &volker]);

    let cross: i64 = benny.store.conn.query_row(
        "SELECT COUNT(*) FROM events e JOIN tasks t ON t.id = e.task_id WHERE e.project_id != t.project_id", [], |r| r.get(0)).unwrap();
    assert_eq!(cross, 0, "no event on the shared board points at the private one");
    let waits = task_by_title(&benny, "waits on theirs");
    assert_eq!(waits.blocked_by, None, "a blocker that left the room here is not linked across boards");
    assert!(benny.store.task_detail(private, here.id).unwrap().events.iter().all(|e| e.body != "still on it"));
}
