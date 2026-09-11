//! The events table doubles as the change feed.
//!
//! `docs/http-api.md` has the HTTP server detect changes by polling `MAX(events.id)` and
//! telling clients to refetch. That works only while **every mutation writes an event** --
//! a write that skips one is simply invisible to a live page, with no error anywhere.
//!
//! It nearly comes for free: priority #1 already forced every task and note mutation to
//! record history, so the audit log and the change feed are the same table. The memory
//! requirement pays for the live UI. But "nearly" is the dangerous part, which is what
//! `every_mutation_moves_the_change_cursor` is here to pin down -- it failed for project
//! creation and path learning until #13.
//!
//! The second half is that these new events must not reach the *agent*. The board's
//! `recent` section is eight lines of the most expensive space in the product, and
//! "learned path /Users/x/repo/src/core" is not an answer to "what happened here lately".

use ai_kanban::core::model::*;
use ai_kanban::core::note::{NoteDraft, NotePatch};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::Store;

/// What the HTTP server polls.
fn cursor(s: &Store) -> i64 {
    s.change_cursor().unwrap()
}

#[test]
fn every_mutation_moves_the_change_cursor() {
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();

    let mut last = cursor(&s);
    let mut moved = |s: &Store, what: &str| {
        let now = cursor(s);
        assert!(now > last, "{what} wrote no event, so a live page would never see it");
        last = now;
    };

    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    moved(&s, "creating a project");

    let sub = dir.path().join("src");
    std::fs::create_dir_all(&sub).unwrap();
    s.resolve_project(&sub).unwrap();
    moved(&s, "learning a path alias");

    let t = s.create_task(pid, TaskDraft::new("a task")).unwrap();
    moved(&s, "creating a task");

    s.update_task(pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();
    moved(&s, "updating a task");

    let n = s.create_note(pid, NoteDraft::new("a note"), Actor::Agent).unwrap();
    moved(&s, "creating a note");

    s.update_note(pid, n.id, NotePatch { body: Some("more".into()), ..Default::default() }).unwrap();
    moved(&s, "updating a note");

    s.log(pid, Actor::Agent, "a decision").unwrap();
    moved(&s, "logging");

    let w = s.ensure_workstream(pid, "contact-form").unwrap();
    moved(&s, "creating a workstream");

    s.set_current_workstream(pid, w.id).unwrap();
    moved(&s, "entering a workstream");

    s.clear_current_workstream(pid).unwrap();
    moved(&s, "leaving a workstream");

    s.close_workstream(pid, w.id).unwrap();
    moved(&s, "closing a workstream");

    let checkout = dir.path().join("a-checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let r = s.add_repo(pid, &checkout, None, Actor::User).unwrap();
    moved(&s, "registering a repo");

    s.rename_repo(pid, r.id, "renamed", Actor::User).unwrap();
    moved(&s, "renaming a repo");

    s.update_task(pid, t.id, TaskPatch { repos: Some(vec![r.id]), ..Default::default() }).unwrap();
    moved(&s, "linking a task to a repo");

    s.update_task(pid, t.id, TaskPatch { planio: Some(Some(48213)), ..Default::default() }).unwrap();
    moved(&s, "setting a Planio ref");

    s.remove_repo(pid, r.id, Actor::User).unwrap();
    moved(&s, "removing a repo");

    let board = s.create_board("BMUKN").unwrap();
    moved(&s, "creating a board");

    let shared_dir = dir.path().join("shared");
    std::fs::create_dir_all(&shared_dir).unwrap();
    let shared = s.add_repo(pid, &shared_dir, None, Actor::User).unwrap();
    s.add_repo(board.id, &shared_dir, None, Actor::User).unwrap();
    moved(&s, "sharing a repo with a second board");

    s.set_repo_home(board.id, shared.id, Actor::User).unwrap();
    moved(&s, "moving a repo's home");

    s.move_task(pid, t.id, board.id, Actor::User, None, None).unwrap();
    moved(&s, "moving a task to another board");

    s.forget_repo(shared.id).unwrap();
    moved(&s, "forgetting a repo");

    s.set_profile_allowed(pid, "claude-work", false, Actor::User).unwrap();
    moved(&s, "refusing a session profile on a board");

    s.set_profile_allowed(pid, "claude-work", true, Actor::User).unwrap();
    moved(&s, "allowing it again");

    // The last one on purpose: it deletes the board's events, which is exactly what could
    // leave MAX(id) standing still or going backwards.
    s.forget_board(board.id).unwrap();
    moved(&s, "forgetting a board");
}

#[test]
fn a_save_that_changes_nothing_writes_nothing() {
    // The web form sends every field on every save. Recording those as edits filled the
    // history with "no change" lines and bumped the version under anyone else's open form.
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    let t = s.create_task(pid, TaskDraft::new("a task")).unwrap();
    let before = cursor(&s);

    let same = s.update_task(pid, t.id, TaskPatch {
        title: Some("a task".into()), status: Some(t.status), priority: Some(t.priority),
        tags: Some(vec![]), repos: Some(vec![]), planio: Some(None), workstream: Some(None),
        expected_version: Some(t.version),
        ..Default::default()
    }).unwrap();
    assert_eq!(cursor(&s), before, "an unchanged save must not reach the history");
    assert_eq!(same.version, t.version);

    // A reason on its own is a comment, and that is history.
    s.update_task(pid, t.id, TaskPatch { log: Some("checked, still valid".into()), ..Default::default() }).unwrap();
    assert!(cursor(&s) > before);
}

#[test]
fn a_path_is_only_recorded_the_first_time_it_is_seen() {
    // This runs on every resolve. If it wrote an event each time rather than only when the
    // path is genuinely new, a single project would accumulate one event per session per
    // directory -- history made entirely of noise.
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();

    s.resolve_project(dir.path()).unwrap();
    let after_first = cursor(&s);

    s.resolve_project(dir.path()).unwrap();
    assert_eq!(cursor(&s), after_first, "re-resolving a known path must write nothing");
}

#[test]
fn path_events_stay_out_of_the_agents_board() {
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;

    // Several new subdirectories, as a real session wanders through a repo.
    for sub in ["src", "src/core", "docs", "tests"] {
        let p = dir.path().join(sub);
        std::fs::create_dir_all(&p).unwrap();
        s.resolve_project(&p).unwrap();
    }
    s.create_task(pid, TaskDraft::new("the actual work")).unwrap();

    let board = s.board(pid, &BoardQuery::board()).unwrap();
    let kinds: Vec<&str> = board.recent.iter().map(|e| e.kind.as_str()).collect();

    assert!(!kinds.contains(&"path_learned"), "housekeeping must not spend the agent's `recent` budget");
    assert!(kinds.contains(&"created"), "real work still shows: {kinds:?}");
}

#[test]
fn walking_through_a_project_does_not_make_it_look_active() {
    // `last_activity` drives "last active 3d ago" in project summaries. If path learning
    // counted, a project an agent merely passed through would report as freshly worked on,
    // and the multi-project overview would rank noise above real work.
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;

    // Backdate everything written so far, so anything new is distinguishable.
    s.conn.execute("UPDATE events SET ts = 1000", []).unwrap();
    let before = s.last_activity(pid).unwrap();

    let sub = dir.path().join("vendor");
    std::fs::create_dir_all(&sub).unwrap();
    s.resolve_project(&sub).unwrap();

    assert_eq!(s.last_activity(pid).unwrap(), before, "learning a path is not activity");
}

#[test]
fn recall_does_not_surface_the_record_of_visiting_a_directory() {
    // A `path_learned` body is a filesystem path. Unfiltered, searching for a directory
    // name would return the fact that it was visited instead of the work done there.
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;

    let sub = dir.path().join("middleware");
    std::fs::create_dir_all(&sub).unwrap();
    s.resolve_project(&sub).unwrap();

    let hits = s.recall(&ai_kanban::core::recall::RecallQuery {
        text: "middleware",
        project_id: Some(pid),
        limit: 10,
    }).unwrap();

    assert!(
        hits.hits.iter().all(|h| !h.snippet.contains("middleware") || h.kind != HitKind::Event),
        "a path-learning event must never be a search result"
    );
}

#[test]
fn event_ids_are_never_reused_after_a_delete() {
    // The cursor only works if ids climb. A plain INTEGER PRIMARY KEY is the rowid, and
    // SQLite hands the largest deleted rowid back to the next insert -- so deleting the
    // newest event and writing one more would leave MAX(id) unchanged, and a polling client
    // would conclude nothing happened. Migration 004 made the column AUTOINCREMENT.
    let s = Store::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;

    let high = s.log(pid, Actor::Agent, "the newest event").unwrap();
    s.conn.execute("DELETE FROM events WHERE id = ?1", [high]).unwrap();

    let next = s.log(pid, Actor::Agent, "written after the delete").unwrap();
    assert!(next > high, "id {next} was reused after deleting {high}; the cursor would stall");
}
