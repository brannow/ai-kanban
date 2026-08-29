//! Permanent removal.
//!
//! `a_forgotten_note_is_gone_from_search_too` is the test this feature exists for. Deleting
//! the row is the easy half and proves nothing: the content that could not be removed lived
//! in `note_updated` event bodies, which are FTS-indexed and returned by `recall`. A unit
//! test counting rows would pass while the secret stayed findable.

use ai_kanban::core::model::*;
use ai_kanban::core::note::{NoteDraft, NotePatch};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;

fn fixture() -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    (s, pid)
}

fn finds(s: &Store, pid: i64, text: &str) -> usize {
    s.recall(&ai_kanban::core::recall::RecallQuery { text, project_id: Some(pid), limit: 20 })
        .unwrap()
        .hits
        .len()
}

#[test]
fn a_forgotten_note_is_gone_from_search_too() {
    let (s, pid) = fixture();

    let n = s.create_note(
        pid,
        NoteDraft { body: "the deploy key is AKIAsecret123".into(), ..NoteDraft::new("deploy") },
        Actor::Agent,
    ).unwrap();
    // Overwriting is what a person tries first, and it is what put the content beyond reach:
    // the old body is now inside a note_updated event.
    s.update_note(pid, n.id, NotePatch { body: Some("(see the vault)".into()), ..Default::default() }).unwrap();
    assert_eq!(finds(&s, pid, "AKIAsecret123"), 1, "precondition: overwriting alone does not remove it");

    s.forget_note(pid, n.id).unwrap();

    assert_eq!(finds(&s, pid, "AKIAsecret123"), 0, "the superseded body must be unfindable");
    assert!(s.note(pid, n.id).is_err(), "the note itself is gone");
}

#[test]
fn the_tombstone_names_the_note_and_quotes_nothing() {
    // A tombstone that repeated any part of what it recorded the removal of would undo the
    // operation it is recording.
    let (s, pid) = fixture();
    let n = s.create_note(
        pid,
        NoteDraft { body: "AKIAsecret123".into(), ..NoteDraft::new("a revealing title") },
        Actor::Agent,
    ).unwrap();

    s.forget_note(pid, n.id).unwrap();

    let recent = s.board(pid, &BoardQuery::board()).unwrap().recent;
    let tomb = recent.iter().find(|e| e.kind == "forgotten").expect("a removal is still history");
    assert_eq!(tomb.body, format!("note #{}", n.id));
    assert!(!tomb.body.contains("AKIAsecret123"));
    assert!(!tomb.body.contains("a revealing title"));
}

#[test]
fn forgetting_a_task_keeps_what_was_learned_doing_it() {
    // Notes are a separate entity precisely because knowledge outlives the work. Forgetting
    // the task must not quietly take the note -- or the note's own history -- with it.
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("chase the redirect loop")).unwrap();
    let n = s.create_note(
        pid,
        NoteDraft { task_id: Some(t.id), body: "it was a trailing slash in nginx".into(), ..NoteDraft::new("root cause") },
        Actor::Agent,
    ).unwrap();
    s.update_note(pid, n.id, NotePatch { body: Some("it was a trailing slash in the nginx location block".into()), ..Default::default() }).unwrap();

    s.forget_task(pid, t.id).unwrap();

    let kept = s.note(pid, n.id).expect("the note survives");
    assert_eq!(kept.task_id, None, "it detaches rather than dangling");
    assert!(finds(&s, pid, "nginx") > 0, "and so does its history");
    assert!(s.task(pid, t.id).is_err(), "the task is gone");
}

#[test]
fn forgetting_a_task_removes_its_own_history() {
    let (s, pid) = fixture();
    let t = s.create_task(pid, TaskDraft::new("a task")).unwrap();
    s.update_task(pid, t.id, TaskPatch {
        status: Some(Status::Doing),
        log: Some("starting because the deadline moved".into()),
        ..Default::default()
    }).unwrap();
    assert!(finds(&s, pid, "deadline") > 0);

    s.forget_task(pid, t.id).unwrap();

    assert_eq!(finds(&s, pid, "deadline"), 0, "the task's reasoning goes with it");
}

#[test]
fn forgetting_a_blocker_leaves_the_blocked_task_usable() {
    // `blocked_by` is an annotation, so a dangling id would render as a blocker that cannot
    // be looked up -- a board that describes a state nobody can inspect.
    let (s, pid) = fixture();
    let blocker = s.create_task(pid, TaskDraft::new("the blocker")).unwrap();
    let blocked = s.create_task(pid, TaskDraft {
        blocked_by: Some(blocker.id),
        ..TaskDraft::new("waiting on it")
    }).unwrap();

    s.forget_task(pid, blocker.id).unwrap();

    let after = s.task(pid, blocked.id).unwrap();
    assert_eq!(after.blocked_by, None);
    s.task_detail(pid, blocked.id).unwrap();
}

#[test]
fn a_removal_is_visible_to_the_live_stream() {
    // A delete lowers no MAX(id), so without the tombstone a forget would be invisible to a
    // web page watching the cursor -- it would keep showing a note that no longer exists.
    let (s, pid) = fixture();
    let n = s.create_note(pid, NoteDraft::new("transient"), Actor::Agent).unwrap();
    let before: i64 = s.conn.query_row("SELECT MAX(id) FROM events", [], |r| r.get(0)).unwrap();

    s.forget_note(pid, n.id).unwrap();

    let after: i64 = s.conn.query_row("SELECT MAX(id) FROM events", [], |r| r.get(0)).unwrap();
    assert!(after > before, "the cursor must move so a live page refetches");
}

#[test]
fn forgetting_is_scoped_to_one_board() {
    // The store is global. An unscoped forget would let one board destroy another's history
    // while the response named the caller's own project.
    let s = Store::open_in_memory().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let pa = s.resolve_project(a.path()).unwrap().project.id;
    let pb = s.resolve_project(b.path()).unwrap().project.id;

    let theirs = s.create_note(pb, NoteDraft::new("another board's knowledge"), Actor::Agent).unwrap();

    assert!(s.forget_note(pa, theirs.id).is_err(), "must refuse across boards");
    assert!(s.note(pb, theirs.id).is_ok(), "and must not have removed it");
}
