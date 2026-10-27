//! A room with members, for tests: each member's store flushes into a real relay running
//! in-process and applies what it pulls back. No network.

#![allow(dead_code)]

use ai_kanban::core::model::*;
use ai_kanban::core::Store;
use ai_kanban_relay::{Principal, ServerDb};
use ai_kanban::sync::protocol::Change;

pub const ROOM: &str = "sbahn";

pub struct Member {
    pub store: Store,
    pub pid: i64,
    pub p: Principal,
}

pub fn relay() -> ServerDb {
    let db = ServerDb::open_in_memory().unwrap();
    for u in ["benny", "volker", "carla"] {
        db.add_user(u).unwrap();
    }
    db
}

/// A member whose board `board` is connected to the room, with an origin tag issued by `relay`.
pub fn member(relay: &ServerDb, user: &str, tag: &str, board: &str) -> Member {
    let key = relay.add_key(user, tag, "").unwrap();
    let p = relay.authenticate(&key).unwrap();
    relay.join(ROOM, &p).unwrap();
    let store = Store::open_in_memory().unwrap();
    store.set_origin_tag(tag).unwrap();
    let pid = store.create_board(board).unwrap().id;
    store.connect_board(pid, ROOM).unwrap();
    Member { store, pid, p }
}

pub fn push(relay: &ServerDb, m: &Member) {
    m.store.flush().unwrap();
    loop {
        let out = m.store.outbox(m.pid, 1000).unwrap();
        let Some((last, _)) = out.last() else { break };
        let changes: Vec<Change> = out.iter().map(|(_, c)| c.clone()).collect();
        relay.push(ROOM, &m.p, &changes).unwrap();
        m.store.outbox_sent(m.pid, *last).unwrap();
    }
}

pub fn pull(relay: &ServerDb, m: &Member) {
    loop {
        let since = m.store.sync_status(m.pid).unwrap().unwrap().cursor;
        let page = relay.pull(ROOM, &m.p, since, 1000).unwrap();
        if page.changes.is_empty() {
            break;
        }
        m.store.apply(m.pid, &page.changes).unwrap();
        if !page.more {
            break;
        }
    }
}

pub fn sync(relay: &ServerDb, m: &Member) {
    push(relay, m);
    pull(relay, m);
}

/// Everyone pushes, then everyone pulls, until nothing moves.
pub fn converge(relay: &ServerDb, members: &[&Member]) {
    for _ in 0..3 {
        for m in members {
            push(relay, m);
        }
        for m in members {
            pull(relay, m);
        }
    }
}

pub fn task_by_title(m: &Member, title: &str) -> Task {
    m.store.board(m.pid, &BoardQuery::board().with_status(Status::ALL.to_vec())).unwrap()
        .tasks.into_iter().find(|t| t.title == title)
        .unwrap_or_else(|| panic!("no task {title:?} on {}", m.p.tag))
}

