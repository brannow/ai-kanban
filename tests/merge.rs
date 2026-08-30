//! Repairing a board that already split.
//!
//! The failure being repaired is silent: two half-boards, neither of which errors, and an
//! agent starting cold reads one and concludes that is all there is. So the assertions here
//! are about *what survived*, and in particular about the references that only exist
//! because of a split -- a task on one half blocked by a task on the other.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;

/// Two boards that should have been one. Built the way a real split happens: a second
/// clone resolves to its own identity and gets worked on independently.
fn split(dir: &std::path::Path) -> (Store, i64, i64) {
    let s = Store::open(&dir.join("store.db")).unwrap();
    let original = dir.join("repo");
    let clone = dir.join("repo-clone");
    std::fs::create_dir_all(&original).unwrap();
    std::fs::create_dir_all(&clone).unwrap();

    let a = s.resolve_project(&original).unwrap().project.id;
    let b = s.resolve_project(&clone).unwrap().project.id;
    assert_ne!(a, b, "the fixture must actually be split");

    s.create_task(a, TaskDraft { status: Status::Doing, ..TaskDraft::new("on the original") }).unwrap();
    s.create_task(b, TaskDraft::new("on the clone")).unwrap();
    s.create_note(b, NoteDraft::new("learned on the clone"), Actor::Agent).unwrap();
    s.log(b, Actor::Agent, "history that must not vanish").unwrap();
    (s, a, b)
}

#[test]
fn everything_moves_and_the_second_board_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());

    let report = s.merge_projects(a, b).unwrap();
    assert_eq!(report.tasks, 1);
    assert_eq!(report.notes, 1);
    assert!(report.paths >= 1);

    assert_eq!(s.all_projects().unwrap().len(), 1, "the merged-in board must be gone");
    let board = s.board(a, &BoardQuery::board()).unwrap();
    let titles: Vec<_> = board.tasks.iter().map(|t| t.title.as_str()).collect();
    assert!(titles.contains(&"on the original"));
    assert!(titles.contains(&"on the clone"), "the clone's work must survive the merge");
}

#[test]
fn a_blocker_that_spanned_the_split_survives() {
    // The reference that only exists because of a split, and the one a naive
    // delete-then-move would destroy: `tasks.blocked_by` is ON DELETE SET NULL, so removing
    // the source project before reparenting would quietly null it out. The board would look
    // fine and would have lost the dependency.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());

    let blocker = s.create_task(a, TaskDraft::new("the blocker")).unwrap();
    let blocked = s
        .create_task(b, TaskDraft { status: Status::Blocked, blocked_by: Some(blocker.id),
            ..TaskDraft::new("waiting across the split") })
        .unwrap();

    s.merge_projects(a, b).unwrap();

    let after = s.task(a, blocked.id).unwrap();
    assert_eq!(after.blocked_by, Some(blocker.id), "the cross-board blocker was lost");
}

#[test]
fn the_merged_boards_history_is_kept_not_just_its_tasks() {
    // Priority #1 of the project is persistent memory. Moving tasks while dropping the
    // events would keep the work and lose the reasoning, which is the more valuable half.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());

    s.merge_projects(a, b).unwrap();
    let recent = s.recent_events(a, 50).unwrap();
    assert!(recent.iter().any(|e| e.body.contains("history that must not vanish")));
}

#[test]
fn the_merge_explains_itself_in_the_boards_history() {
    // Not housekeeping on purpose: this is what explains why task ids have gaps and why two
    // narratives interleave, to an agent reading `recent` cold months later.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());

    let before = s.change_cursor().unwrap();
    s.merge_projects(a, b).unwrap();

    assert!(s.change_cursor().unwrap() > before, "a merge is invisible to the live feed");
    let recent = s.recent_events(a, 50).unwrap();
    assert!(
        recent.iter().any(|e| e.kind == "project_merged"),
        "the merge must appear in recent, not only in the store"
    );
}

#[test]
fn both_directories_now_lead_to_the_surviving_board() {
    // The point of the repair. Until the aliases move, the next session opened in the clone
    // mints the split all over again.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());
    s.merge_projects(a, b).unwrap();

    for dir in ["repo", "repo-clone"] {
        let resolved = s.resolve_project(&tmp.path().join(dir)).unwrap().project.id;
        assert_eq!(resolved, a, "{dir} still resolves somewhere else");
    }
    assert_eq!(s.all_projects().unwrap().len(), 1, "resolving must not have re-split the board");
}

#[test]
fn the_older_board_wins_when_nobody_chooses() {
    // The original is the board with history worth keeping and the key other things already
    // resolve against; the split is the accident.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());
    let older = s.project(a).unwrap();

    let report = s.merge_projects_auto(b, a).unwrap();
    assert_eq!(report.into.id, older.id, "the newer board should not have survived");
}

#[test]
fn merging_a_board_into_itself_is_refused() {
    // Without the guard this deletes the project after moving its rows onto itself, which
    // is a board that erases itself on a typo.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, _) = split(tmp.path());
    assert!(s.merge_projects(a, a).is_err());
    assert_eq!(s.all_projects().unwrap().len(), 2, "a refused merge must change nothing");
}

#[test]
fn notes_stay_findable_by_their_files_after_moving() {
    // Reparenting fires the FTS update triggers. Worth asserting rather than assuming: a
    // note that survives the move but drops out of the index is memory that exists and can
    // never be recalled, which is indistinguishable from having lost it.
    let tmp = tempfile::tempdir().unwrap();
    let (s, a, b) = split(tmp.path());
    s.create_note(b, NoteDraft {
        paths: vec!["src/core/store.rs".into()],
        ..NoteDraft::new("the store opens in WAL mode")
    }, Actor::Agent).unwrap();

    s.merge_projects(a, b).unwrap();

    let hits = s.notes_for_path(a, "src/core/store.rs").unwrap();
    assert!(hits.iter().any(|n| n.title.contains("WAL mode")), "note lost its path binding");

    let recalled = s
        .recall(&ai_kanban::core::recall::RecallQuery { text: "WAL mode", project_id: Some(a), limit: 10 })
        .unwrap();
    assert!(!recalled.hits.is_empty(), "the note survived the merge but left the search index");
}
