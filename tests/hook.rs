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

// ---------------------------------------------------------------------------
// PostToolUse -- contextual recall
// ---------------------------------------------------------------------------

use ai_kanban::core::note::NoteDraft as ND;

fn note_about(s: &Store, pid: i64, title: &str, body: &str, paths: &[&str]) -> i64 {
    s.create_note(pid, ND {
        body: body.into(),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        ..ND::new(title)
    }, Actor::Agent).unwrap().id
}

#[test]
fn a_note_surfaces_for_the_file_it_is_about() {
    // The whole point of note_paths, and the half that decides whether memory is used at
    // all: recall assumes the agent thinks "let me search my memory", and it will not.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    note_about(&s, pid, "middleware rewrites redirects",
        "The rewrite runs before the guard, so the guard never fires.", &["src/auth/middleware.rs"]);

    let hit = s.notes_for_path(pid, &format!("{}/src/auth/middleware.rs", tmp.path().display())).unwrap();
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].title, "middleware rewrites redirects");
}

#[test]
fn a_note_does_not_surface_for_a_different_file_with_the_same_name() {
    // A plain suffix match makes a note filed against auth.rs fire for
    // vendor/other/auth.rs. A note surfacing on the wrong file is worse than no note: it is
    // a confident claim about code it was never about.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    note_about(&s, pid, "our auth", "internal detail", &["src/auth.rs"]);

    let root = tmp.path().display();
    assert_eq!(s.notes_for_path(pid, &format!("{root}/src/auth.rs")).unwrap().len(), 1);
    // A path that genuinely ends in the stored one, at a boundary, IS a match.
    assert_eq!(s.notes_for_path(pid, &format!("{root}/vendor/other/src/auth.rs")).unwrap().len(), 1,
        "a real suffix match at a directory boundary must still hit");
    // The cases that must NOT match.
    assert!(s.notes_for_path(pid, &format!("{root}/vendor/auth.rs")).unwrap().is_empty(),
        "same basename in another directory is not the same file");
    assert!(s.notes_for_path(pid, &format!("{root}/src/notauth.rs")).unwrap().is_empty(),
        "suffix matching must respect the directory boundary");
}

#[test]
fn an_underscore_in_a_filename_is_not_a_wildcard() {
    // `_` is LIKE's single-character wildcard and underscores in filenames are ordinary,
    // so an unescaped pattern makes a note about auth_guard.rs fire on authXguard.rs.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    note_about(&s, pid, "about the guard", "detail", &["src/auth_guard.rs"]);

    let root = tmp.path().display();
    assert_eq!(s.notes_for_path(pid, &format!("{root}/src/auth_guard.rs")).unwrap().len(), 1);
    assert!(s.notes_for_path(pid, &format!("{root}/src/authXguard.rs")).unwrap().is_empty(),
        "_ must be a literal underscore, not a wildcard");
}

#[test]
fn a_file_with_nothing_recorded_produces_nothing() {
    // This fires on every read and edit in the session. Anything printed that was not worth
    // printing is a tax on every tool call.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    note_about(&s, pid, "about one file", "detail", &["src/auth.rs"]);

    assert!(s.notes_for_path(pid, &format!("{}/src/unrelated.rs", tmp.path().display())).unwrap().is_empty());
}

#[test]
fn notes_do_not_leak_between_projects() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pa = s.resolve_project(a.path()).unwrap().project.id;
    let pb = s.resolve_project(b.path()).unwrap().project.id;
    note_about(&s, pa, "project A knowledge", "detail", &["src/main.rs"]);

    assert_eq!(s.notes_for_path(pa, &format!("{}/src/main.rs", a.path().display())).unwrap().len(), 1);
    assert!(s.notes_for_path(pb, &format!("{}/src/main.rs", b.path().display())).unwrap().is_empty());
}

#[test]
fn the_newest_note_for_a_file_comes_first() {
    // Only a few are shown per hit, so ordering decides which ones the agent actually sees.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    let old = note_about(&s, pid, "older claim", "a", &["src/x.rs"]);
    let new = note_about(&s, pid, "newer claim", "b", &["src/x.rs"]);
    s.update_note(pid, new, ai_kanban::core::note::NotePatch {
        body: Some("b revised".into()), ..Default::default()
    }).unwrap();

    let hits = s.notes_for_path(pid, &format!("{}/src/x.rs", tmp.path().display())).unwrap();
    assert_eq!(hits[0].id, new);
    assert_eq!(hits[1].id, old);
}

#[test]
fn one_note_covering_several_files_is_returned_once_per_file() {
    // DISTINCT matters: a note listing three paths must not appear three times for one file.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(tmp.path()).unwrap().project.id;
    note_about(&s, pid, "spans the auth layer", "detail",
        &["src/auth.rs", "src/auth/middleware.rs", "src/auth/guard.rs"]);

    let hits = s.notes_for_path(pid, &format!("{}/src/auth/guard.rs", tmp.path().display())).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].paths.len(), 3, "the note still knows every file it covers");
}
