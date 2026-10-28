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

fn tags(m: &Member, task: i64) -> Vec<String> {
    m.store.claimers(m.pid, task).unwrap().into_iter().map(|c| c.tag).collect()
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

    assert_eq!(tags(&benny, t.id), ["v1"]);
    let b = text(&benny);
    assert!(b.contains("auth timeout behind the proxy") && b.contains("by volker"), "{b}");
    assert!(b.contains("team: b1 benny (you), v1 volker (1 doing)"), "{b}");
    let v = text(&volker);
    assert!(v.contains("by you"), "{v}");
    assert!(v.contains("b1 benny, v1 volker (you, 1 doing)"), "{v}");
}

#[test]
fn joining_a_teammate_on_a_doing_task_is_seen_by_the_teammate_too() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("proxy timeout")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "proxy timeout");
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);

    // Already in doing, so the status does not change -- but benny joining is a claim, and the
    // one already working on it is the person who most needs to hear about it.
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), claim: true, ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    know_the_team(&benny);
    know_the_team(&volker);
    assert_eq!(tags(&volker, vt.id), ["v1", "b1"]);
    let v = text(&volker);
    assert!(v.contains("by you, benny"), "{v}");
    assert!(v.contains("b1 benny (1 doing), v1 volker (you, 1 doing)"), "{v}");
    assert!(text(&benny).contains("by volker, you"));

    // Claiming again changes nothing: no second entry in the history.
    let before = benny.store.task_events(t.id).unwrap().len();
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), claim: true, ..patch() }).unwrap();
    assert_eq!(benny.store.task_events(t.id).unwrap().len(), before);
}

#[test]
fn saving_a_teammates_doing_task_from_a_form_is_not_a_claim() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("form save")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "form save");
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Doing), claim: true, ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    // The web form sends every field, status included, on every save: a body edit says nothing
    // about who is working on it.
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), body: Some("more detail".into()), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    assert_eq!(tags(&volker, vt.id), ["v1"]);
}

#[test]
fn joining_a_task_filed_straight_into_doing_keeps_whoever_filed_it() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    volker.store.create_task(volker.pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("filed into doing") }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let t = task_by_title(&benny, "filed into doing");
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), claim: true, ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "filed into doing");
    assert_eq!(tags(&benny, t.id), ["v1", "b1"]);
    assert_eq!(tags(&volker, vt.id), ["v1", "b1"]);
}

#[test]
fn two_claims_in_the_same_moment_both_stay_visible() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("same moment")).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "same moment");
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let mut b = tags(&benny, t.id);
    let mut v = tags(&volker, vt.id);
    b.sort();
    v.sort();
    assert_eq!((b, v), (vec!["b1".to_string(), "v1".into()], vec!["b1".to_string(), "v1".into()]));
}

#[test]
fn leaving_doing_ends_every_claim_and_the_next_one_starts_fresh() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    let t = benny.store.create_task(benny.pid, TaskDraft::new("round two")).unwrap();
    benny.store.update_task(benny.pid, t.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let vt = task_by_title(&volker, "round two");
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Doing), claim: true, ..patch() }).unwrap();
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Blocked), ..patch() }).unwrap();
    volker.store.update_task(volker.pid, vt.id, TaskPatch { status: Some(Status::Doing), ..patch() }).unwrap();
    converge(&relay, &[&benny, &volker]);
    assert_eq!(tags(&benny, t.id), ["v1"], "benny's claim ended when it left doing");
}

#[test]
fn a_task_filed_straight_into_doing_is_claimed_by_whoever_filed_it() {
    let relay = relay();
    let benny = member(&relay, "benny", "b1", "sbb");
    let volker = member(&relay, "volker", "v1", "x");
    volker.store.create_task(volker.pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("already on it") }).unwrap();
    converge(&relay, &[&benny, &volker]);
    let t = task_by_title(&benny, "already on it");
    assert_eq!(tags(&benny, t.id), ["v1"]);
}

#[test]
fn a_private_board_names_no_claims_and_looks_as_ever() {
    let s = Store::open_in_memory().unwrap();
    s.set_origin_tag("b1").unwrap();
    let pid = s.create_board("private").unwrap().id;
    let t = s.create_task(pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("mine") }).unwrap();
    assert!(s.claimers(pid, t.id).unwrap().is_empty());
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
    let w = render::claim_warning("#12-v1", &["volker".into()], 1000, 1000 + 20 * 60);
    assert!(w.starts_with("Heads-up: volker has had #12-v1 in doing since 20m ago."), "{w}");
    assert!(render::claim_warning("#12-v1", &["volker".into()], 1000, 1010).contains("since just now."));
    let two = render::claim_warning("#12-v1", &["volker".into(), "carla".into()], 1000, 1010);
    assert!(two.starts_with("Heads-up: volker and carla have had #12-v1") && two.contains("check with volker and carla"), "{two}");
    assert!(!w.contains("move it back"), "the status is shared: moving it back would take it from volker too");
    let d = render::possible_duplicates(&[("#7-v1".into(), "Login times out".into(), Status::Doing, Some("volker".into()))]);
    assert!(d.contains("#7-v1 Login times out  (doing, by volker)"), "{d}");
    assert_eq!(render::possible_duplicates(&[]), "");
}
