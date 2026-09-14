//! Repos per ticket, and the Planio ref.
//!
//! Note #17's lesson again (see `tests/tags.rs`): a new field on a task has several homes,
//! and most of them fail SILENTLY with a green suite, because every existing test predates
//! the field. These pin each home -- resolution, the patch round trip, the board line, the
//! write response's splice, recall, export/import, merge, forget -- and the read-only hook
//! against a store this binary has not migrated.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
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
    assert!(render::error(&err).contains("repo_add"),
        "an empty board must name the tool that fixes it: {}", render::error(&err));

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
fn a_session_started_from_the_board_is_told_the_ticket_and_where_to_find_it() {
    // The session starts in the ticket's first repo, whose home may be another board -- so the
    // prompt names the board, or the agent's task_show would look on the wrong one.
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft {
        repos: vec![api.id], planio: Some(1234), body: "Autoplay stutters on Safari.".into(),
        ..TaskDraft::new("Rework header slider")
    }).unwrap();
    let prompt = render::start_prompt(&f.s.task_detail(f.pid, t.id).unwrap());
    for want in ["Rework header slider", "Planio issue #1234", "Autoplay stutters", &canon(&f.api), "project \"eee\""] {
        assert!(prompt.contains(want), "missing {want:?} in:\n{prompt}");
    }
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
fn an_import_shares_a_checkout_that_already_opens_on_another_board() {
    // One directory, one home: the import attaches the repo so its tickets keep it, but the
    // folder keeps opening on the board that had it -- and the report says so, because
    // nothing else would.
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
    assert!(render::import_report(&report).contains("shared without moving it"), "a kept home must be said, not silent");
    assert_eq!(target.repos(owner).unwrap()[0].home_project_id, owner, "the owner keeps the home");
    let imported = target.all_projects().unwrap().into_iter().find(|p| p.id != owner).unwrap().id;
    let task = target.board(imported, &BoardQuery::board()).unwrap().tasks[0].clone();
    assert_eq!(names(&target.task_repos(imported, task.id).unwrap()), ["eee-api"], "its ticket keeps the repo");
}

#[test]
fn merging_boards_keeps_repos_their_homes_and_the_tickets_naming_them() {
    // Deleting the merged board cascades to its attachments, to every repo homed there, and
    // to their links. A merge that did not move them first would strip every ticket from that
    // board of its repos, silently.
    let f = fixture();
    let shared = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let b_dir = f.root.path().join("half");
    std::fs::create_dir_all(&b_dir).unwrap();
    let b = f.s.resolve_project(&b_dir).unwrap().project.id;
    let web = f.s.add_repo(b, &f.web, None, Actor::User).unwrap();
    f.s.add_repo(b, &f.api, None, Actor::User).unwrap();
    let t = f.s.create_task(b, TaskDraft { repos: vec![web.id, shared.id], ..TaskDraft::new("contact form spam") }).unwrap();

    let report = f.s.merge_projects(f.pid, b).unwrap();
    assert_eq!(report.repos, 2);
    assert_eq!(names(&f.s.repos(f.pid).unwrap()), ["eee-api", "eee-web"], "a repo on both ends up on the survivor once");
    assert_eq!(names(&f.s.task_repos(f.pid, t.id).unwrap()), ["eee-api", "eee-web"]);
    assert_eq!(f.s.resolve_project(&f.web).unwrap().project.id, f.pid,
        "a repo homed on the merged board now opens on the survivor");
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
         DROP TABLE board_repos;
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

#[test]
fn a_store_between_the_two_repo_migrations_still_renders_its_board_and_its_hook() {
    // 008 added `board_repos`. A store the hook meets at 007 has repos but not that table,
    // and has to render as "tracks no repos" rather than lose its board.
    let f = fixture();
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], ..TaskDraft::new("invoice rounding") }).unwrap();
    f.s.conn.execute_batch("DROP TABLE board_repos;").unwrap();

    assert_eq!(f.s.repo_count(f.pid).unwrap(), 0);
    let text = render::board(&f.s.board(f.pid, &BoardQuery::board()).unwrap());
    assert!(text.contains("invoice rounding") && !text.contains("no repo set"), "{text}");
    assert!(ai_kanban::hook::context_for(&f.s, &f.board_dir).is_some());
}

fn board(f: &Fixture, name: &str) -> i64 {
    f.s.create_board(name).unwrap().id
}

#[test]
fn a_repo_can_serve_several_boards_and_its_folder_opens_on_its_home() {
    let f = fixture();
    let other = board(&f, "BMUKN");
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let shared = f.s.add_repo(other, &f.api, None, Actor::User).unwrap();
    assert_eq!(shared.id, api.id, "the same checkout is the same repo on every board");
    assert_eq!(shared.home_project_id, f.pid, "sharing a repo does not move where its folder opens");

    f.s.create_task(f.pid, TaskDraft { repos: vec![api.id], ..TaskDraft::new("ours") }).unwrap();
    f.s.create_task(other, TaskDraft { repos: vec![api.id], ..TaskDraft::new("theirs") }).unwrap();

    let here = &f.s.repo_summaries(f.pid).unwrap()[0];
    let there = &f.s.repo_summaries(other).unwrap()[0];
    assert_eq!((here.total, there.total), (1, 1), "each board counts its own tickets");
    assert_eq!(here.other_boards, ["BMUKN"]);
    assert_eq!(there.home_board, "eee");
    assert_eq!(f.s.resolve_project(&f.api).unwrap().project.id, f.pid);
}

#[test]
fn moving_the_home_moves_where_the_folder_and_its_subdirectories_open() {
    let f = fixture();
    let other = board(&f, "BMUKN");
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    // A subdirectory the old home learned. Left behind, it would keep answering for the old
    // home -- resolution checks the deepest alias first -- and the move would half happen.
    let src = f.api.join("src");
    std::fs::create_dir_all(&src).unwrap();
    assert_eq!(f.s.resolve_project(&src).unwrap().project.id, f.pid);

    f.s.add_repo(other, &f.api, None, Actor::User).unwrap();
    f.s.set_repo_home(other, api.id, Actor::User).unwrap();

    assert_eq!(f.s.resolve_project(&f.api).unwrap().project.id, other);
    assert_eq!(f.s.resolve_project(&src).unwrap().project.id, other);
    // Read from the events rather than the rendered board: a board with no tasks renders only
    // "No tasks yet", which would hide the line this is checking.
    let old: Vec<String> = f.s.recent_events(f.pid, 8).unwrap().into_iter().map(|e| e.body).collect();
    assert!(old.iter().any(|b| b.contains("now opens on board \"BMUKN\"")), "the board the folder left must say so: {old:?}");
    assert!(f.s.set_repo_home(f.pid, 999, Actor::User).is_err(), "only a repo on this board can be homed here");
}

#[test]
fn when_the_home_lets_a_repo_go_another_of_its_boards_takes_it() {
    // Otherwise the folder would keep opening on a board that no longer has the repo.
    let f = fixture();
    let other = board(&f, "BMUKN");
    let api = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    f.s.add_repo(other, &f.api, None, Actor::User).unwrap();

    f.s.remove_repo(f.pid, api.id, Actor::User).unwrap();
    assert!(f.s.repos(f.pid).unwrap().is_empty());
    assert_eq!(f.s.repos(other).unwrap()[0].home_project_id, other);
    assert_eq!(f.s.resolve_project(&f.api).unwrap().project.id, other);
}

#[test]
fn a_repo_name_means_one_checkout_on_every_board() {
    let f = fixture();
    let other = board(&f, "BMUKN");
    f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    assert!(f.s.add_repo(other, &f.web, Some("eee-api"), Actor::User).is_err());
}

#[test]
fn a_board_is_created_by_name_and_a_name_is_not_reused() {
    // Two boards with one name is what a split looks like in `ai-kanban projects`.
    let f = fixture();
    let b = f.s.create_board("BMUKN").unwrap();
    assert_eq!((b.name.as_str(), b.key.as_str()), ("BMUKN", "board:bmukn"));
    assert!(f.s.create_board("bmukn").is_err());
    assert!(f.s.create_board("eee").is_err(), "the fixture's own board already has that name");
    assert!(f.s.create_board("   ").is_err());
    assert!(f.s.all_projects().unwrap().iter().any(|p| p.id == b.id));
}

#[test]
fn moving_a_ticket_keeps_its_history_and_says_what_it_left_behind() {
    let f = fixture();
    let other = board(&f, "BMUKN");
    let shared = f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    f.s.add_repo(other, &f.api, None, Actor::User).unwrap();
    let local = f.s.add_repo(f.pid, &f.web, None, Actor::User).unwrap();
    let ws = f.s.ensure_workstream(f.pid, "slider").unwrap();
    f.s.set_current_workstream(f.pid, ws.id).unwrap();

    let blocker = f.s.create_task(f.pid, TaskDraft::new("pick a slider library")).unwrap();
    let t = f.s.create_task(f.pid, TaskDraft {
        repos: vec![shared.id, local.id], blocked_by: Some(blocker.id), planio: Some(1234),
        ..TaskDraft::new("Rework header slider")
    }).unwrap();
    let dependent = f.s.create_task(f.pid, TaskDraft { blocked_by: Some(t.id), ..TaskDraft::new("hero copy") }).unwrap();
    let note = f.s.create_note(f.pid, NoteDraft { task_id: Some(t.id), ..NoteDraft::new("the slider is swiper v8") }, Actor::Agent).unwrap();
    f.s.update_task(f.pid, t.id, TaskPatch { log: Some("started on the markup".into()), ..Default::default() }).unwrap();

    let moved = f.s.move_task(f.pid, t.id, other, Actor::User, Some("belongs to BMUKN"), None).unwrap();
    assert_eq!((moved.id, moved.project_id, moved.blocked_by), (t.id, other, None));
    assert!(f.s.task_opt(f.pid, t.id).unwrap().is_none(), "it is gone from the old board");
    assert_eq!(names(&f.s.task_repos(other, t.id).unwrap()), ["eee-api"], "a repo the new board lacks stays behind");
    assert_eq!(f.s.task_planio(other, t.id).unwrap(), Some(1234));
    assert_eq!(f.s.task_workstream(other, t.id).unwrap(), None);
    assert_eq!(f.s.task(f.pid, dependent.id).unwrap().blocked_by, None,
        "a blocker on another board would render as an id nobody here can look up");
    assert!(f.s.note(other, note.id).is_ok(), "notes written on it travel with it");

    let history: Vec<String> = f.s.task_events(t.id).unwrap().into_iter().map(|e| e.body).collect();
    assert!(history.iter().any(|b| b == "started on the markup"), "old history travels: {history:?}");
    let last = history.last().unwrap();
    assert!(last.contains("belongs to BMUKN") && last.contains("eee-web") && last.contains("workstream"), "{last}");

    let left = render::board(&f.s.board(f.pid, &BoardQuery::board()).unwrap());
    assert!(left.contains("moved to board \"BMUKN\""), "the board it left must say where it went: {left}");
    let hits = f.s.recall(&RecallQuery { text: "slider", project_id: Some(other), limit: DEFAULT_LIMIT }).unwrap().hits;
    assert!(hits.iter().any(|h| h.kind == HitKind::Task && h.id == t.id), "{hits:?}");

    let stale = f.s.move_task(other, t.id, f.pid, Actor::User, None, Some(1));
    assert!(matches!(stale, Err(Error::Conflict { .. })), "the web UI's move is version-guarded");
}

#[test]
fn the_repo_menu_shows_the_path_and_only_names_a_home_that_is_elsewhere() {
    // The path is the point: it is what tells an agent which directory the work is in. The
    // home board is noise on the common row and only worth a line when it is NOT this board.
    let f = fixture();
    f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let out = render::repo_menu("eee", &f.s.repo_summaries(f.pid).unwrap());
    assert!(out.contains("eee-api"), "{out}");
    assert!(out.contains(&canon(&f.api)), "{out}");
    assert!(!out.contains("opens on board"), "its home IS this board: {out}");

    // Shared onto a second board: there the row has to say where the folder actually opens,
    // or an agent reads the path as leading back to the board it is looking at.
    let other = f.s.create_board("web").unwrap();
    f.s.add_repo(other.id, &f.api, None, Actor::User).unwrap();
    let out = render::repo_menu("web", &f.s.repo_summaries(other.id).unwrap());
    assert!(out.contains("opens on board \"eee\""), "{out}");
}

#[test]
fn an_empty_board_is_told_how_to_get_its_first_repo() {
    // The case that made every ticket say "no repo set": a board that tracks no repos, and
    // an agent with no idea that registering one is something it may do.
    let f = fixture();
    let out = render::repo_menu("eee", &f.s.repo_summaries(f.pid).unwrap());
    assert!(out.contains("repo_add"), "{out}");
}

#[test]
fn the_cross_board_listing_names_the_board_each_folder_opens_on() {
    let f = fixture();
    f.s.add_repo(f.pid, &f.api, None, Actor::User).unwrap();
    let homes: std::collections::HashMap<i64, String> =
        f.s.all_projects().unwrap().into_iter().map(|p| (p.id, p.name)).collect();
    let repos: Vec<(Repo, String)> = f.s.all_repos().unwrap().into_iter()
        .map(|r| { let h = homes[&r.home_project_id].clone(); (r, h) }).collect();
    let out = render::repo_directory(&repos);
    assert!(out.contains("1 repo\n"), "{out}");
    assert!(out.contains("(opens on eee)"), "{out}");
}

#[test]
fn adding_a_repo_twice_is_not_an_error() {
    // Agents retry. A second `repo_add` of the same checkout has to be the state the caller
    // asked for, not a failure it has to interpret -- and it must not make a second repo.
    let f = fixture();
    let first = f.s.add_repo(f.pid, &f.api, None, Actor::Agent).unwrap();
    let again = f.s.add_repo(f.pid, &f.api, None, Actor::Agent).unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(f.s.repos(f.pid).unwrap().len(), 1);
}
