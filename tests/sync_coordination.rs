//! Who is on what, on a shared board: the claim each `doing` row names, the warning when a claim
//! overlaps someone else's, and the duplicate hint when filing. Derived from history -- there is
//! no assignee column.

mod common;
use common::sync::*;

use ai_kanban::core::model::*;
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;
use ai_kanban::render;

fn patch() -> TaskPatch {
    TaskPatch { actor: Actor::Agent, ..Default::default() }
}

/// What the relay would report as the room's members, so names show instead of tags.
fn know_the_team(m: &Member) {
    let team = vec![("b1".to_string(), "benny".to_string()), ("v1".to_string(), "volker".to_string())];
    m.store.sync_succeeded(m.pid, &team).unwrap();
}

fn text(m: &Member) -> String {
    render::board(&m.store.board(m.pid, &BoardQuery::board()).unwrap())
}

#[test]
fn a_doing_row_names_who_is_on_it_and_the_team_line_counts_it() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("auth timeout behind the proxy")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "auth timeout behind the proxy");
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    know_the_team(&benny);
    know_the_team(&volker);

    assert_eq!(benny.store.claim(benny.pid, t.id).unwrap().unwrap().tag, "v1");
    let b = text(&benny);
    assert!(b.contains("auth timeout behind the proxy") && b.contains("by volker"), "{b}");
    assert!(b.contains("team: b1 benny (you), v1 volker (1 doing)"), "{b}");
    let v = text(&volker);
    assert!(v.contains("by you"), "{v}");
    assert!(v.contains("b1 benny, v1 volker (you, 1 doing)"), "{v}");
}

#[test]
fn a_task_filed_straight_into_doing_is_claimed_by_whoever_filed_it() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    volker.store.create_task(volker.pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("already on it") }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let t = task_by_title(&benny, "already on it");
    assert_eq!(benny.store.claim(benny.pid, t.id).unwrap().unwrap().tag, "v1");
}

#[test]
fn a_private_board_names_no_claims_and_looks_as_ever() {
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("private").unwrap().id;
    let t = s.create_task(pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("mine") }).unwrap();
    assert_eq!(s.claim(pid, t.id).unwrap(), None);
    let text = render::board(&s.board(pid, &BoardQuery::board()).unwrap());
    assert!(!text.contains("by you"), "{text}");
    assert!(s.possible_duplicates(pid, t.id, 2).unwrap().is_empty());
}

#[test]
fn filing_what_a_teammate_already_filed_is_noticed() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    volker.store.create_task(volker.pid, TaskDraft::new("Login times out behind the corporate proxy")).unwrap();
    volker.store.create_task(volker.pid, TaskDraft::new("Dark mode for the settings page")).unwrap();
    let done = volker.store.create_task(volker.pid, TaskDraft::new("Login proxy timeout, the old one")).unwrap();
    volker.store.update_task(volker.pid, done.id, TaskPatch { status: Some(Status::Done), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);

    let mine = benny.store.create_task(benny.pid, TaskDraft::new("proxy: login timeout")).unwrap();
    let dups = benny.store.possible_duplicates(benny.pid, mine.id, 2).unwrap();
    let titles: Vec<&str> = dups.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Login times out behind the corporate proxy"], "finished work is not a duplicate of open work");

    let other = benny.store.create_task(benny.pid, TaskDraft::new("Upgrade the database driver")).unwrap();
    assert!(benny.store.possible_duplicates(benny.pid, other.id, 2).unwrap().is_empty(), "unrelated titles stay quiet");
    let short = benny.store.create_task(benny.pid, TaskDraft::new("proxy")).unwrap();
    assert!(benny.store.possible_duplicates(benny.pid, short.id, 2).unwrap().is_empty(), "one word is not enough to say");
}

#[test]
fn the_warnings_read_as_a_heads_up_with_the_handle_and_the_name() {
    let w = render::claim_warning("#12-v1", "volker", 1000, 1000 + 20 * 60);
    assert!(w.starts_with("Heads-up: volker has had #12-v1 in doing since 20m ago."), "{w}");
    assert!(render::claim_warning("#12-v1", "volker", 1000, 1010).contains("since just now."));
    let d = render::possible_duplicates(&[("#7-v1".into(), "Login times out".into(), Status::Doing, Some("volker".into()))]);
    assert!(d.contains("#7-v1 Login times out  (doing, by volker)"), "{d}");
    assert_eq!(render::possible_duplicates(&[]), "");
}
