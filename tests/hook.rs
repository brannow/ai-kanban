//! The session-start hook.
//!
//! Two of these guard properties that are invisible until they have already done damage:
//! a hook that creates boards turns the store into a record of every directory the user has
//! visited, and a hook that exceeds the host's output cap gets its board truncated
//! mid-render.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;
use ai_kanban::hook;

#[test]
fn a_known_board_arrives_with_its_state_and_what_to_do_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    s.create_task(pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("MCP tool surface") }).unwrap();

    let ctx = hook::context_for(&s, tmp.path()).unwrap();

    assert!(ctx.contains("MCP tool surface"), "the board state must be there");
    // The guidance is the half that causes adoption; state alone does not.
    assert!(ctx.contains("task_add"));
    assert!(ctx.contains("recall"));
}

#[test]
fn a_directory_with_no_board_produces_nothing_at_all() {
    // Two properties in one. The hook must not mint a board -- otherwise every scratch
    // folder and tarball opened in Claude Code becomes a project. And it must stay silent
    // rather than suggest starting one: the store exists after the first use of ai-kanban,
    // so any nudge here would appear in every directory forever.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();

    assert!(hook::context_for(&s, tmp.path()).is_none(), "nothing to say means say nothing");
    assert!(s.all_projects().unwrap().is_empty(), "the hook must not create anything");
}

#[test]
fn a_subdirectory_sees_its_projects_board() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    s.create_task(pid, TaskDraft::new("the work")).unwrap();
    let deep = tmp.path().join("src").join("core");
    std::fs::create_dir_all(&deep).unwrap();

    assert!(hook::context_for(&s, &deep).unwrap().contains("the work"));
}

#[test]
fn the_context_stays_under_the_hosts_output_cap() {
    // Claude Code caps hook output at 10,000 characters and truncates past it, which would
    // cut a board mid-row. The board's own caps are what keep this true, so this test fails
    // if someone widens them without thinking about the hook.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    for i in 0..300 {
        s.create_task(pid, TaskDraft {
            body: format!("a long description for task {i} ").repeat(6),
            ..TaskDraft::new(format!("Task {i}: something with a fairly long descriptive title"))
        }).unwrap();
    }
    for i in 0..50 {
        s.create_note(pid, NoteDraft::new(format!("note {i}")), Actor::Agent).unwrap();
    }

    let ctx = hook::context_for(&s, tmp.path()).unwrap();
    eprintln!("session-start context: {} chars", ctx.len());
    assert!(ctx.len() < 10_000, "context was {} chars", ctx.len());
}
