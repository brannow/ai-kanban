//! Optimistic concurrency on tasks and notes.
//!
//! The case this exists for: a human has a task open in a browser form for two minutes
//! while an agent moves the same task. Without a guard the later write wins silently *and*
//! both writes land in the event log, so the history reads as though the board contradicted
//! itself. On a project whose first priority is history, corrupting the record is worse than
//! refusing the write.
//!
//! `a_refused_update_leaves_no_trace` is the one that matters most: a guard that blocks the
//! row change but still writes its event would put a change in the history that never
//! happened, which is the exact damage the guard was added to prevent.

use ai_kanban::core::model::*;
use ai_kanban::core::note::{NoteDraft, NotePatch};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::{Error, Store};

fn fixture() -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let p = s.resolve_project(tmp.path()).unwrap();
    // The TempDir is dropped here on purpose: the project row is already written, and none
    // of these tests touch the filesystem again.
    (s, p.project.id)
}

#[test]
fn a_new_task_starts_at_version_one() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("first")).unwrap();
    assert_eq!(t.version, 1);
}

#[test]
fn every_update_bumps_the_version() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("first")).unwrap();

    let t = s.update_task(pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();
    assert_eq!(t.version, 2);
    let t = s.update_task(pid, t.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();
    assert_eq!(t.version, 3);
}

#[test]
fn an_agent_update_needs_no_version() {
    // The MCP path sends None. If this ever required a version, `task_update` would have to
    // be preceded by a `task_show` to fetch one -- two calls for one intent, which is the
    // rule the whole tool surface is built on.
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("filed by an agent")).unwrap();

    let updated = s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Doing),
        expected_version: None,
        ..Default::default()
    }).unwrap();

    assert_eq!(updated.status, Status::Doing);
}

#[test]
fn a_guarded_update_holding_the_current_version_succeeds() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("the work")).unwrap();

    let updated = s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Doing),
        expected_version: Some(t.version),
        ..Default::default()
    }).unwrap();

    assert_eq!(updated.status, Status::Doing);
    assert_eq!(updated.version, t.version + 1);
}

#[test]
fn a_guarded_update_holding_a_stale_version_is_refused() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("the work")).unwrap();
    let stale = t.version;

    // Somebody else -- an agent, say -- moves it while the browser form sits open.
    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();

    let err = s.update_task(pid, t.id, TaskPatch {
        title: Some("edited in a form opened two minutes ago".into()),
        expected_version: Some(stale),
        ..Default::default()
    }).unwrap_err();

    match err {
        // Both versions ride along so the caller can say what happened rather than only
        // refusing -- the HTTP layer renders this as 412 with the current state.
        Error::Conflict { id, expected, actual } => {
            assert_eq!(id, t.id);
            assert_eq!(expected, stale);
            assert_eq!(actual, stale + 1);
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn a_refused_update_leaves_no_trace() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("the work")).unwrap();
    let stale = t.version;
    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();

    let events_before = s.task_events(t.id).unwrap().len();

    let _ = s.update_task(pid, t.id, TaskPatch {
        title: Some("never happened".into()),
        expected_version: Some(stale),
        ..Default::default()
    }).unwrap_err();

    let after = s.task(pid, t.id).unwrap();
    assert_eq!(after.title, "the work", "the row must be untouched");
    assert_eq!(after.status, Status::Doing);
    assert_eq!(
        s.task_events(t.id).unwrap().len(),
        events_before,
        "a refused write must not appear in the history -- recording a change that did not \
         happen is the damage this guard exists to prevent"
    );
}

#[test]
fn notes_carry_the_same_guard() {
    let (s, pid) = fixture();
    let n = s.create_note(pid, NoteDraft::new("what we learned"), Actor::Agent).unwrap();
    let stale = n.version;

    s.update_note(pid, n.id, NotePatch { body: Some("refined".into()), ..Default::default() }).unwrap();

    let err = s.update_note(pid, n.id, NotePatch {
        body: Some("from a stale form".into()),
        expected_version: Some(stale),
        ..Default::default()
    }).unwrap_err();

    assert!(matches!(err, Error::Conflict { .. }), "got {err:?}");
    assert_eq!(s.note(pid, n.id).unwrap().body, "refined");
}
