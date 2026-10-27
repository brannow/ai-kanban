//! Task and note ids are never handed out twice (migration 013, task #1064).
//!
//! A forgotten task's id coming back means every old reference to it -- "blocked by #812" in
//! a log, a commit message -- silently names a different task. Under sync it would also issue
//! the same handle twice. Every path that deletes rows and every path that inserts them is
//! covered, because a single one that skips the floor brings the bug back.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;

fn fixture() -> (Store, i64, tempfile::TempDir) {
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    (s, pid, dir)
}

fn finds(s: &Store, text: &str) -> usize {
    s.recall(&ai_kanban::core::recall::RecallQuery { text, project_id: None, limit: 20 }).unwrap().hits.len()
}

#[test]
fn forgetting_the_newest_task_does_not_hand_its_id_to_the_next() {
    let (s, pid, _d) = fixture();
    s.create_task(pid, TaskDraft::new("first")).unwrap();
    let newest = s.create_task(pid, TaskDraft::new("newest")).unwrap();
    s.forget_task(pid, newest.id).unwrap();

    let next = s.create_task(pid, TaskDraft::new("next")).unwrap();
    assert!(next.id > newest.id, "#{} reused the forgotten #{}", next.id, newest.id);
    // And the one after that simply follows on.
    let after = s.create_task(pid, TaskDraft::new("after")).unwrap();
    assert_eq!(after.id, next.id + 1);
}

#[test]
fn forgetting_the_newest_note_does_not_hand_its_id_to_the_next() {
    let (s, pid, _d) = fixture();
    let newest = s.create_note(pid, NoteDraft::new("newest"), Actor::Agent).unwrap();
    s.forget_note(pid, newest.id).unwrap();
    let next = s.create_note(pid, NoteDraft::new("next"), Actor::Agent).unwrap();
    assert!(next.id > newest.id);
}

#[test]
fn a_moved_id_keeps_everything_attached_to_it_and_stays_searchable() {
    let (s, pid, dir) = fixture();
    let newest = s.create_task(pid, TaskDraft::new("doomed")).unwrap();
    s.forget_task(pid, newest.id).unwrap();

    let checkout = dir.path().join("svc");
    std::fs::create_dir_all(&checkout).unwrap();
    let repo = s.add_repo(pid, &checkout, Some("svc"), Actor::User).unwrap();
    let draft = TaskDraft { repos: vec![repo.id], ..TaskDraft::new("zebracorn rollout") };
    let t = s.create_task(pid, draft).unwrap();

    assert!(t.id > newest.id);
    assert_eq!(s.task_repos(pid, t.id).unwrap().len(), 1, "repo link written under the final id");
    let detail = s.task_detail(pid, t.id).unwrap();
    assert!(detail.events.iter().any(|e| e.kind == "created"), "its created event points at the final id");
    assert_eq!(finds(&s, "zebracorn"), 1, "the FTS row followed the id change");
    assert_eq!(finds(&s, "doomed"), 0);
}

#[test]
fn forgetting_a_whole_board_does_not_free_its_ids_for_another_board() {
    let s = Store::open_in_memory().unwrap();
    let a = s.create_board("a").unwrap();
    let b = s.create_board("b").unwrap();
    let t = s.create_task(a.id, TaskDraft::new("on a")).unwrap();
    let n = s.create_note(a.id, NoteDraft::new("on a"), Actor::Agent).unwrap();
    s.forget_board(a.id).unwrap();

    assert!(s.create_task(b.id, TaskDraft::new("on b")).unwrap().id > t.id);
    assert!(s.create_note(b.id, NoteDraft::new("on b"), Actor::Agent).unwrap().id > n.id);
}

#[test]
fn an_import_after_a_forget_does_not_reuse_ids_either() {
    let (s, pid, _d) = fixture();
    s.create_task(pid, TaskDraft::new("kept")).unwrap();
    let export = s.export(&[]).unwrap();

    let other = Store::open_in_memory().unwrap();
    let scratch = other.create_board("scratch").unwrap();
    let doomed = other.create_task(scratch.id, TaskDraft::new("doomed")).unwrap();
    other.forget_task(scratch.id, doomed.id).unwrap();
    other.import(&export).unwrap();

    let imported = other.all_projects().unwrap().into_iter().find(|p| p.id != scratch.id).unwrap();
    let ids: Vec<i64> = other.board(imported.id, &BoardQuery::board()).unwrap().tasks.iter().map(|t| t.id).collect();
    assert!(ids.iter().all(|&id| id > doomed.id), "{ids:?} vs forgotten #{}", doomed.id);
}

#[test]
fn deleting_an_older_task_never_lowers_the_floor() {
    let (s, pid, _d) = fixture();
    let old = s.create_task(pid, TaskDraft::new("old")).unwrap();
    let newest = s.create_task(pid, TaskDraft::new("newest")).unwrap();
    s.forget_task(pid, newest.id).unwrap();
    s.forget_task(pid, old.id).unwrap();
    assert!(s.create_task(pid, TaskDraft::new("next")).unwrap().id > newest.id);
}

/// The floor starts at the store's current maximum, so a store that upgrades with tasks in it
/// keeps counting upward from them.
#[test]
fn an_upgraded_store_starts_its_floor_at_the_highest_existing_id() {
    let (s, pid, _d) = fixture();
    let a = s.create_task(pid, TaskDraft::new("a")).unwrap();
    let floor: i64 = s.conn.query_row("SELECT value FROM id_floor WHERE tbl = 'tasks'", [], |r| r.get(0)).unwrap();
    assert!(floor < a.id, "a fresh store starts below its first task");

    s.conn.execute("DELETE FROM id_floor", []).unwrap();
    s.conn.execute_batch(include_str!("../src/core/migrations/013_id_floor.sql")).unwrap();
    let floor: i64 = s.conn.query_row("SELECT value FROM id_floor WHERE tbl = 'tasks'", [], |r| r.get(0)).unwrap();
    assert_eq!(floor, a.id);
}
