//! Repos per ticket, and the Planio ref.
//!
//! Note #17's lesson again (see `tests/tags.rs`): a new field on a task has several homes,
//! and most of them fail SILENTLY with a green suite, because every existing test predates
//! the field. These pin each home -- resolution, the patch round trip, the board line, the
//! write response's splice, recall, export/import, merge, forget -- and the read-only hook
//! against a store this binary has not migrated.

use ai_kanban::core::model::*;
use ai_kanban::core::recall::{RecallQuery, DEFAULT_LIMIT};
use ai_kanban::core::task::{TaskDraft, TaskPatch};
use ai_kanban::core::{Error, Store};
use ai_kanban::render;
use std::path::PathBuf;

/// A board at `<root>/eee` and two sibling checkouts, `eee-api` and `eee-web`, as a Planio
/// project with two repositories would look on disk.
struct Fixture {
    s: Store,
    pid: i64,
    root: tempfile::TempDir,
    board_dir: PathBuf,
    api: PathBuf,
    web: PathBuf,
}

fn fixture() -> Fixture {
    let s = Store::open_in_memory().unwrap();
    let root = tempfile::tempdir().unwrap();
    let board_dir = root.path().join("eee");
    let api = root.path().join("eee-api");
    let web = root.path().join("eee-web");
    for d in [&board_dir, &api, &web] {
        std::fs::create_dir_all(d).unwrap();
    }
    let pid = s.resolve_project(&board_dir).unwrap().project.id;
    Fixture { s, pid, root, board_dir, api, web }
}

fn canon(p: &std::path::Path) -> String {
    std::fs::canonicalize(p).unwrap().to_string_lossy().into_owned()
}

fn names(repos: &[Repo]) -> Vec<String> {
    repos.iter().map(|r| r.name.clone()).collect()
}

/// The board line for one task. Matched on the padded id column, so `#1` cannot match `#12`.
fn line_for(text: &str, id: i64) -> String {
    let needle = format!("#{id:<4} ");
    text.lines().find(|l| l.contains(&needle)).unwrap_or_else(|| panic!("no line for #{id} in:\n{text}")).to_string()
}

#[test]
fn registering_a_repo_makes_its_folder_resolve_to_the_board() {
    // The half of "a board owns its repos" an agent actually feels: opening any of the
    // board's checkouts lands it on the board holding that checkout's tickets.
    let f = fixture();
    f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();

    let deep = f.api.join("src/invoice");
    std::fs::create_dir_all(&deep).unwrap();
    assert_eq!(f.s.resolve_project(&deep).unwrap().project.id, f.pid);
    // And the read-only path the SessionStart hook uses agrees, or the cold-start board would
    // be missing in exactly the repo the ticket is about.
    assert!(f.s.find_project(&f.web).unwrap().is_none(), "an unregistered sibling is not claimed");
    assert_eq!(f.s.find_project(&f.api).unwrap().unwrap().id, f.pid);
}

#[test]
fn a_folder_another_board_already_claims_is_refused_and_the_owner_named() {
    // Resolution checks the deepest alias first, so taking the root while another board
    // holds a subdirectory would silently not take effect -- and taking the subdirectory too
    // would move that board's sessions out from under its history.
    let f = fixture();
    let sub = f.web.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let other = f.s.resolve_project(&sub).unwrap().project;

    let err = f.s.add_repo(f.pid, &f.web, None, Actor::User).unwrap_err();
    let text = render::error(&err);
    match err {
        Error::PathClaimed { board, key, .. } => {
            assert_eq!(board, other.name);
            assert_eq!(key, other.key);
        }
        e => panic!("expected PathClaimed, got {e:?}"),
    }
    assert!(text.contains("ai-kanban merge"), "the refusal must say what to do instead: {text}");
    assert!(f.s.repos(f.pid).unwrap().is_empty(), "a refused registration writes nothing");
}

#[test]
fn registering_twice_returns_the_same_repo_and_any_spelling_finds_it() {
    let f = fixture();
    let a = f.s.add_repo(f.pid, &f.api, Some("EEE_API"), Actor::User).unwrap();
    assert_eq!(a.name, "eee-api", "names are normalized like workstreams, because agents type them");
    assert_eq!(a.path, canon(&f.api));

    let again = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    assert_eq!(again.id, a.id, "asking for a state that already holds is not an error");

    assert_eq!(f.s.resolve_repos(f.pid, &["Eee Api".into()]).unwrap(), vec![a.id]);
    assert_eq!(f.s.resolve_repos(f.pid, &[f.api.to_string_lossy().into_owned()]).unwrap(), vec![a.id],
        "a path names the repo too");
}

#[test]
fn a_ticket_touches_many_repos_and_a_repo_carries_many_tickets() {
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let web = f.s.add_repo(f.pid, &f.web, None, Actor::User).unwrap();

    let both = f.s.create_task(f.pid, TaskDraft { repos: vec![web.id, api.id], ..TaskDraft::new("invoice rounding") }).unwrap();
    let one = f.s.create_task(f.pid, TaskDraft { repos: vec![web.id], ..TaskDraft::new("contact form spam") }).unwrap();

    assert_eq!(names(&f.s.task_repos(f.pid, both.id).unwrap()), ["eee-api", "eee-web"]);
    let web_row = |s: &Store| s.repo_summaries(f.pid).unwrap().into_iter().find(|r| r.repo.id == web.id).unwrap();
    assert_eq!((web_row(&f.s).open, web_row(&f.s).total), (2, 2));

    f.s.update_task(f.pid, one.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();
    assert_eq!((web_row(&f.s).open, web_row(&f.s).total), (1, 2), "the menu counts open work separately");
}

#[test]
fn repos_and_planio_survive_patches_about_only_them_and_patches_about_something_else() {
    // `TaskPatch::is_empty` lists every field by hand and `update_task` returns early on it.
    // A field missing there makes a patch changing only that field a silent no-op -- it has
    // shipped broken that way once already (`workstream`).
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft::new("invoice rounding")).unwrap();

    f.s.update_task(f.pid, t.id, TaskPatch { repos: Some(vec![api.id]), ..Default::default() }).unwrap();
    assert_eq!(names(&f.s.task_repos(f.pid, t.id).unwrap()), ["eee-api"]);
    f.s.update_task(f.pid, t.id, TaskPatch { planio: Some(Some(48213)), ..Default::default() }).unwrap();
    assert_eq!(f.s.task_planio(f.pid, t.id).unwrap(), Some(48213));

    // The update rewrites every column, so anything read back wrong would be wiped by an
    // unrelated status change.
    f.s.update_task(f.pid, t.id, TaskPatch { status: Some(Status::Doing), ..Default::default() }).unwrap();
    assert_eq!(names(&f.s.task_repos(f.pid, t.id).unwrap()), ["eee-api"]);
    assert_eq!(f.s.task_planio(f.pid, t.id).unwrap(), Some(48213));

    // With no `log`, the history names what changed, by name.
    let history: Vec<String> = f.s.task_events(t.id).unwrap().into_iter().map(|e| e.body).collect();
    assert!(history.iter().any(|b| b.contains("repos: eee-api")), "{history:?}");
    assert!(history.iter().any(|b| b.contains("planio #48213")), "{history:?}");

    f.s.update_task(f.pid, t.id, TaskPatch { repos: Some(vec![]), planio: Some(None), ..Default::default() }).unwrap();
    assert!(f.s.task_repos(f.pid, t.id).unwrap().is_empty());
    assert_eq!(f.s.task_planio(f.pid, t.id).unwrap(), None);

    assert!(f.s.update_task(f.pid, t.id, TaskPatch { planio: Some(Some(-4)), ..Default::default() }).is_err(),
        "a Planio number is positive");
}

#[test]
fn a_ticket_cannot_name_another_boards_repo() {
    // The same-board guard `blocked_by` and workstreams have: the store is global, and an id
    // from another board would link a ticket to a checkout its board does not own.
    let f = fixture();
    let other_dir = f.root.path().join("other");
    let other_repo = f.root.path().join("other-repo");
    std::fs::create_dir_all(&other_dir).unwrap();
    std::fs::create_dir_all(&other_repo).unwrap();
    let other = f.s.resolve_project(&other_dir).unwrap().project.id;
    let theirs = f.s.add_repo(other, &other_repo, None, Actor::User).unwrap();

    assert!(f.s.create_task(f.pid, TaskDraft { repos: vec![theirs.id], ..TaskDraft::new("x") }).is_err());
    let t = f.s.create_task(f.pid, TaskDraft::new("ours")).unwrap();
    assert!(f.s.update_task(f.pid, t.id, TaskPatch { repos: Some(vec![theirs.id]), ..Default::default() }).is_err());
    assert!(f.s.resolve_repos(f.pid, &["other-repo".into()]).is_err());
}

#[test]
fn an_unknown_repo_lists_the_ones_that_exist_or_says_where_they_come_from() {
    let f = fixture();
    let err = f.s.resolve_repos(f.pid, &["eee-api".into()]).unwrap_err();
    assert!(render::error(&err).contains("repos menu"),
        "an agent cannot add repos, so an empty board must say who does: {}", render::error(&err));

    f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let err = f.s.resolve_repos(f.pid, &["eee-apx".into()]).unwrap_err();
    assert!(render::error(&err).contains("eee-api"), "{}", render::error(&err));
}

#[test]
fn the_board_line_names_repos_and_planio_and_flags_open_tickets_with_none() {
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let linked = f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], planio: Some(48213), ..TaskDraft::new("invoice rounding") }).unwrap();
    let bare = f.s.create_task(f.pid, TaskDraft::new("contact form spam")).unwrap();
    let finished = f.s.create_task(f.pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("old work") }).unwrap();
    f.s.update_task(f.pid, finished.id, TaskPatch { status: Some(Status::Done), ..Default::default() }).unwrap();

    let text = render::board(&f.s.board(f.pid, &BoardQuery::board()).unwrap());
    let l = line_for(&text, linked.id);
    assert!(l.contains("[eee-api]") && l.contains("planio 48213"), "{l}");
    assert!(line_for(&text, bare.id).contains("no repo set"), "{text}");

    // Finished work is not asked for repos -- nobody is about to start it.
    let done = render::board(&f.s.board(f.pid, &BoardQuery::board().with_status(vec![Status::Done])).unwrap());
    assert!(!line_for(&done, finished.id).contains("no repo set"), "{done}");
}

#[test]
fn a_board_that_tracks_no_repos_says_nothing_about_them() {
    // Every board that predates this feature -- including this repo's own. Flagging all of
    // them would put the same words on every line, which is how a reader learns to skip one.
    let f = fixture();
    let t = f.s.create_task(f.pid, TaskDraft::new("anything")).unwrap();
    let text = render::board(&f.s.board(f.pid, &BoardQuery::board()).unwrap());
    assert!(!text.contains("no repo set"), "{text}");
    let detail = render::task_detail(&f.s.task_detail(f.pid, t.id).unwrap());
    assert!(!detail.contains("repo"), "{detail}");
}

#[test]
fn task_show_gives_the_paths_or_says_to_ask() {
    // task_show is where an agent commits to the work, so it is where it learns WHERE the
    // work is -- the name alone does not say which checkout to open.
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], planio: Some(48213), ..TaskDraft::new("invoice rounding") }).unwrap();
    let shown = render::task_detail(&f.s.task_detail(f.pid, t.id).unwrap());
    assert!(shown.contains(&canon(&f.api)), "{shown}");
    assert!(shown.contains("planio #48213"), "{shown}");

    let bare = f.s.create_task(f.pid, TaskDraft::new("contact form spam")).unwrap();
    let shown = render::task_detail(&f.s.task_detail(f.pid, bare.id).unwrap());
    assert!(shown.contains("no repo set") && shown.contains("ask the user"), "{shown}");
}

#[test]
fn a_write_response_shows_the_repos_of_the_task_it_confirms() {
    // A filed backlog task is not in the write response's doing/blocked filter; it is spliced
    // in afterwards. The links were computed before the splice, so without recomputing them
    // the one row this response exists to confirm would be the one missing its repos.
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    f.s.create_task(f.pid, TaskDraft { status: Status::Doing, ..TaskDraft::new("in flight") }).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], planio: Some(7), ..TaskDraft::new("side quest") }).unwrap();

    let text = render::board(&f.s.board_after_mutation(f.pid, t.id).unwrap());
    let l = line_for(&text, t.id);
    assert!(l.contains("[eee-api]") && l.contains("planio 7"), "{text}");
}

#[test]
fn a_planio_number_is_findable_by_recall() {
    // tasks_fts was rebuilt to index it. An empty or stale FTS index returns no rows rather
    // than an error, so only a search proves the rebuild and the triggers are right.
    let f = fixture();
    let t = f.s.create_task(f.pid, TaskDraft { planio: Some(48213), ..TaskDraft::new("invoice rounding") }).unwrap();
    let hits = |s: &Store, q: &str| s.recall(&RecallQuery { text: q, project_id: Some(f.pid), limit: DEFAULT_LIMIT }).unwrap().hits;

    let h = hits(&f.s, "48213");
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(h[0].id, t.id);

    f.s.update_task(f.pid, t.id, TaskPatch { planio: Some(None), ..Default::default() }).unwrap();
    assert!(hits(&f.s, "48213").is_empty(), "the index must forget a cleared ref");
    assert_eq!(hits(&f.s, "rounding").len(), 1, "the rebuild kept the columns that already worked");
}

#[test]
fn repos_planio_and_links_survive_export_and_import() {
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    f.s.add_repo(f.pid, &f.web, None, Actor::User).unwrap();
    f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], planio: Some(48213), ..TaskDraft::new("invoice rounding") }).unwrap();
    let dump = f.s.export(&[]).unwrap();

    let target = Store::open_in_memory().unwrap();
    target.import(&dump).unwrap();
    let tpid = target.all_projects().unwrap()[0].id;
    assert_eq!(names(&target.repos(tpid).unwrap()), ["eee-api", "eee-web"],
        "a repo no ticket names yet is still registered");
    let task = &target.board(tpid, &BoardQuery::board()).unwrap().tasks[0];
    assert_eq!(names(&target.task_repos(tpid, task.id).unwrap()), ["eee-api"]);
    assert_eq!(target.task_planio(tpid, task.id).unwrap(), Some(48213));
}

#[test]
fn an_import_leaves_a_checkout_with_the_board_that_already_registered_it() {
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], ..TaskDraft::new("invoice rounding") }).unwrap();
    let dump = f.s.export(&[]).unwrap();

    let target = Store::open_in_memory().unwrap();
    let mine = f.root.path().join("mine");
    std::fs::create_dir_all(&mine).unwrap();
    let owner = target.resolve_project(&mine).unwrap().project.id;
    target.add_repo(owner, &f.api, None, Actor::User).unwrap();

    let report = target.import(&dump).unwrap();
    assert_eq!(report.projects[0].repos_skipped, vec![canon(&f.api)]);
    assert!(render::import_report(&report).contains("repo already registered"), "a skip must be said, not silent");
    assert_eq!(target.repos(owner).unwrap().len(), 1, "the owner keeps it");
}

#[test]
fn merging_boards_keeps_repos_and_the_tickets_naming_them() {
    // Deleting the merged board cascades to its repos and their links. A merge that did not
    // move them first would strip every ticket from that board of its repos, silently.
    let f = fixture();
    f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();

    let b_dir = f.root.path().join("half");
    let b_api = f.root.path().join("half-api");
    std::fs::create_dir_all(&b_dir).unwrap();
    std::fs::create_dir_all(&b_api).unwrap();
    let b = f.s.resolve_project(&b_dir).unwrap().project.id;
    let web = f.s.add_repo(b, &f.web, None, Actor::User).unwrap();
    // Same NAME as the survivor's repo, different checkout: must not be folded into it.
    f.s.add_repo(b, &b_api, Some("eee-api"), Actor::User).unwrap();
    let t = f.s.create_task(b, TaskDraft { repos: vec![web.id], ..TaskDraft::new("contact form spam") }).unwrap();

    let report = f.s.merge_projects(f.pid, b).unwrap();
    assert_eq!(report.repos, 2);
    let merged = names(&f.s.repos(f.pid).unwrap());
    assert_eq!(merged.len(), 3, "{merged:?}");
    assert!(merged.contains(&"eee-web".to_string()));
    assert_eq!(names(&f.s.task_repos(f.pid, t.id).unwrap()), ["eee-web"]);
}

#[test]
fn forgetting_a_ticket_takes_it_off_its_repos() {
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], ..TaskDraft::new("invoice rounding") }).unwrap();
    f.s.forget_task(f.pid, t.id).unwrap();
    assert_eq!(f.s.repo_summaries(f.pid).unwrap()[0].total, 0, "a forgotten ticket must stop counting");
}

#[test]
fn removing_a_repo_unlinks_it_but_the_folder_stays_on_the_board() {
    // Dropping the alias would send the next session in that folder to a brand-new, empty
    // board -- the split the whole store is built to prevent.
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], ..TaskDraft::new("invoice rounding") }).unwrap();

    assert_eq!(f.s.remove_repo(f.pid, api.id, Actor::User).unwrap(), 1);
    assert!(f.s.task_repos(f.pid, t.id).unwrap().is_empty());
    assert_eq!(f.s.resolve_project(&f.api).unwrap().project.id, f.pid);
    let recent = render::board(&f.s.board(f.pid, &BoardQuery::board()).unwrap());
    assert!(recent.contains("removed repo eee-api"), "{recent}");
}

#[test]
fn a_store_from_before_repos_still_renders_its_board_and_its_hook() {
    // The SessionStart hook opens the store READ-ONLY and never migrates it. Against a store
    // older than 007 the repo tables and `tasks.planio` do not exist; every hook path is
    // `.ok()?`, so a query naming them would not error anywhere visible -- it would silently
    // cost the agent its cold-start board. Dropping them reproduces that store.
    let f = fixture();
    f.s.create_task(f.pid, TaskDraft::new("work from before repos")).unwrap();
    f.s.conn.execute_batch(
        // The FTS table and its triggers name `planio`, so they go before the column can.
        "DROP TRIGGER tasks_ai; DROP TRIGGER tasks_ad; DROP TRIGGER tasks_au;
         DROP TABLE tasks_fts;
         DROP TABLE task_repos;
         DROP TABLE repos;
         ALTER TABLE tasks DROP COLUMN planio;",
    ).unwrap();

    assert_eq!(f.s.repo_count(f.pid).unwrap(), 0);
    let snap = f.s.board(f.pid, &BoardQuery::board()).unwrap();
    assert_eq!(snap.tasks.len(), 1);
    assert!(snap.links.is_empty());
    assert!(ai_kanban::hook::context_for(&f.s, &f.board_dir).is_some(),
        "the SessionStart hook must still produce output");
}
