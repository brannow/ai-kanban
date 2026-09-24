//! Tests for the failure this project cannot afford: one project resolving to two
//! identities, splitting memory into two half-boards with nothing to signal it.
//!
//! These build git fixtures by writing `.git/config` directly rather than shelling out to
//! `git`. That keeps the tests fast and hermetic, and it exercises the exact parsing the
//! resolver does.

use ai_kanban::core::project::{normalize_remote, Resolution};
use ai_kanban::core::model::Actor;
use ai_kanban::core::{Error, Store};
use std::fs;
use std::path::Path;

fn store() -> Store {
    Store::open_in_memory().expect("in-memory store")
}

/// A directory that looks like a git repo, optionally with an `origin` remote.
fn fake_repo(root: &Path, remote: Option<&str>) {
    let git = root.join(".git");
    fs::create_dir_all(&git).unwrap();
    let mut cfg = String::from("[core]\n\trepositoryformatversion = 0\n");
    if let Some(url) = remote {
        cfg.push_str(&format!("[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*\n"));
    }
    fs::write(git.join("config"), cfg).unwrap();
}

#[test]
fn same_directory_resolves_to_the_same_project() {
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("git@github.com:me/repo.git"));
    let s = store();

    let first = s.resolve_project(tmp.path()).unwrap();
    let second = s.resolve_project(tmp.path()).unwrap();

    assert_eq!(first.project.id, second.project.id);
    assert!(first.created, "first call creates the project");
    assert!(!second.created, "second call must not create a second one");
    // The second call must not redo the filesystem walk -- the learned alias wins.
    assert_eq!(second.how, Resolution::KnownPath);
}

#[test]
fn subdirectory_joins_the_repo_root_project() {
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("https://github.com/me/repo.git"));
    let deep = tmp.path().join("src").join("core");
    fs::create_dir_all(&deep).unwrap();
    let s = store();

    let root = s.resolve_project(tmp.path()).unwrap();
    let nested = s.resolve_project(&deep).unwrap();

    assert_eq!(root.project.id, nested.project.id, "a subdirectory is not its own board");
}

#[test]
fn ssh_and_https_clones_of_one_repo_share_a_board() {
    // The least obvious way memory splits: clone over SSH on the desktop, HTTPS on the
    // laptop, and end up with two boards for one codebase.
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    fake_repo(a.path(), Some("git@github.com:me/repo.git"));
    fake_repo(b.path(), Some("https://github.com/me/repo"));
    let s = store();

    let one = s.resolve_project(a.path()).unwrap();
    let two = s.resolve_project(b.path()).unwrap();

    assert_eq!(one.project.id, two.project.id);
    assert_eq!(two.how, Resolution::GitRemote);
}

#[test]
fn a_worktree_joins_the_main_repo_board() {
    // In a worktree, `.git` is a FILE pointing into the main repo. Mishandled, the
    // worktree becomes its own project -- exactly what project_paths exists to prevent.
    let main = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    fake_repo(main.path(), Some("git@github.com:me/repo.git"));

    let wt_gitdir = main.path().join(".git").join("worktrees").join("feature");
    fs::create_dir_all(&wt_gitdir).unwrap();
    fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
    fs::write(wt.path().join(".git"), format!("gitdir: {}\n", wt_gitdir.display())).unwrap();

    let s = store();
    let m = s.resolve_project(main.path()).unwrap();
    let w = s.resolve_project(wt.path()).unwrap();

    assert_eq!(m.project.id, w.project.id, "worktree must not fork the board");
}

#[test]
fn marker_file_overrides_the_git_root() {
    // The monorepo case: one repo, several packages that want separate boards.
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("git@github.com:me/monorepo.git"));
    let pkg = tmp.path().join("packages").join("api");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join(".ai-kanban"), "# board for the api package\nmonorepo/api\n").unwrap();

    let s = store();
    let root = s.resolve_project(tmp.path()).unwrap();
    let package = s.resolve_project(&pkg).unwrap();

    assert_ne!(root.project.id, package.project.id);
    assert_eq!(package.how, Resolution::Marker);
    assert_eq!(package.project.key, "monorepo/api");
}

#[test]
fn a_plain_directory_still_gets_a_board() {
    // No git, no marker. Must still work -- requiring a repo would be setup overhead.
    let tmp = tempfile::tempdir().unwrap();
    let s = store();
    let r = s.resolve_project(tmp.path()).unwrap();
    assert_eq!(r.how, Resolution::Directory);
    assert!(r.created);
}

#[test]
fn git_repo_without_a_remote_keys_on_its_root_path() {
    // ai-kanban's own situation right now, which is why it is tested rather than assumed.
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), None);
    let s = store();
    let r = s.resolve_project(tmp.path()).unwrap();
    assert_eq!(r.how, Resolution::GitRoot);
    assert!(r.project.key.starts_with("path:"));
}

#[test]
fn every_resolved_path_is_learned_as_an_alias() {
    // Alias learning is the mitigation for split memory; if it silently stopped working
    // nothing else would notice.
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("git@github.com:me/repo.git"));
    let deep = tmp.path().join("a").join("b");
    fs::create_dir_all(&deep).unwrap();
    let s = store();

    let r = s.resolve_project(&deep).unwrap();
    let paths = s.project_paths(r.project.id).unwrap();

    let deep_c = std::fs::canonicalize(&deep).unwrap();
    let root_c = std::fs::canonicalize(tmp.path()).unwrap();
    assert!(paths.iter().any(|p| Path::new(p) == deep_c), "starting path learned");
    assert!(paths.iter().any(|p| Path::new(p) == root_c), "derived root learned too");
}

#[test]
fn remote_url_spellings_collapse_to_one_key() {
    let expected = "github.com/me/repo";
    for url in [
        "git@github.com:me/repo.git",
        "https://github.com/me/repo.git",
        "https://github.com/me/repo",
        "ssh://git@github.com/me/repo.git",
        "https://user:token@github.com/me/repo.git",
        "git@github.com:me/repo",
        "https://GitHub.com/Me/Repo.git",
    ] {
        assert_eq!(normalize_remote(url), expected, "failed for {url}");
    }
}

#[test]
fn different_repos_stay_different() {
    // The inverse failure: over-eager normalization merging two real projects into one
    // board would be worse than splitting, because it is not reversible by aliasing.
    assert_ne!(normalize_remote("git@github.com:me/repo.git"), normalize_remote("git@github.com:me/other.git"));
    assert_ne!(normalize_remote("git@github.com:me/repo.git"), normalize_remote("git@gitlab.com:me/repo.git"));
}

#[test]
fn a_subdirectory_of_a_non_git_project_joins_its_board() {
    // The git case works because `.git` marks the root. With no repo and no marker there is
    // nothing to anchor on, so every subdirectory used to derive its own identity -- making
    // ~/notes and ~/notes/drafts two separate memories. Learned aliases are checked at each
    // level of the walk precisely to close that.
    let tmp = tempfile::tempdir().unwrap();
    let deep = tmp.path().join("drafts").join("2026");
    fs::create_dir_all(&deep).unwrap();
    let s = store();

    let root = s.resolve_project(tmp.path()).unwrap();
    let nested = s.resolve_project(&deep).unwrap();

    assert_eq!(root.project.id, nested.project.id, "a plain subdirectory must not fork the board");
    assert_eq!(nested.how, Resolution::KnownPath);
    assert!(!nested.created);
}

#[test]
fn a_marker_still_wins_over_an_alias_inherited_from_above() {
    // The inverse risk of checking aliases during the walk: a repo-wide board sitting above
    // a monorepo package could swallow it. The marker is checked first at every level.
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("git@github.com:me/monorepo.git"));
    let pkg = tmp.path().join("packages").join("api");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join(".ai-kanban"), "monorepo/api\n").unwrap();
    let s = store();

    // Resolve the repo-wide board FIRST, so its alias is already learned and sitting above.
    let root = s.resolve_project(tmp.path()).unwrap();
    let package = s.resolve_project(&pkg).unwrap();
    let below_marker = s.resolve_project(&pkg.join("src")).unwrap();

    assert_ne!(root.project.id, package.project.id);
    assert_eq!(package.how, Resolution::Marker);
    assert_eq!(below_marker.project.id, package.project.id, "a directory under the marker belongs to the package");
}

#[test]
fn the_read_only_lookup_never_creates_a_board() {
    // The session-start hook runs in every directory the user opens Claude Code in. If it
    // created boards, the store would become a record of where they have been.
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("git@github.com:me/repo.git"));
    let s = store();

    assert!(s.find_project(tmp.path()).unwrap().is_none());
    assert!(s.all_projects().unwrap().is_empty(), "a lookup must not mint a board");

    // Once the board genuinely exists, the same lookup finds it -- from a subdirectory too.
    let created = s.resolve_project(tmp.path()).unwrap();
    let deep = tmp.path().join("src");
    fs::create_dir_all(&deep).unwrap();
    assert_eq!(s.find_project(&deep).unwrap().unwrap().id, created.project.id);
    assert_eq!(s.all_projects().unwrap().len(), 1);
}

#[test]
fn a_marker_added_after_the_fact_still_takes_effect() {
    // The realistic sequence: work happens in a monorepo package, and only later does
    // someone decide it deserves its own board. If the alias learned during that work
    // short-circuits the walk before the marker's level, adding the marker does nothing --
    // an escape hatch that silently stops working, which is worse than not having one.
    let tmp = tempfile::tempdir().unwrap();
    fake_repo(tmp.path(), Some("git@github.com:me/monorepo.git"));
    let pkg = tmp.path().join("packages").join("api");
    let deep = pkg.join("src");
    fs::create_dir_all(&deep).unwrap();
    let s = store();

    // Work happens first, with no marker: everything belongs to the repo-wide board.
    let before = s.resolve_project(&deep).unwrap();
    assert_eq!(before.how, Resolution::GitRemote);

    // The marker arrives afterwards.
    fs::write(pkg.join(".ai-kanban"), "monorepo/api\n").unwrap();
    let after = s.resolve_project(&deep).unwrap();

    assert_eq!(after.how, Resolution::Marker);
    assert_eq!(after.project.key, "monorepo/api");
    assert_ne!(after.project.id, before.project.id);
}

fn refused(r: ai_kanban::core::Result<ai_kanban::core::project::Resolved>) -> bool {
    matches!(r, Err(Error::SharedDirectory { .. }))
}

#[test]
fn the_home_directory_never_becomes_a_board_by_default() {
    // The failure seen on a real store: a session started in $HOME made it a board, and
    // since a known path is matched at every level of the walk, every non-git directory
    // beneath it then joined that one board.
    let home = tempfile::tempdir().unwrap();
    let s = store();

    let r = s.resolve_project_from(home.path(), Some(home.path()));
    let err = r.expect_err("$HOME must be refused");
    assert!(matches!(err, Error::SharedDirectory { .. }), "got {err:?}");
    assert!(s.all_projects().unwrap().is_empty(), "a refused resolve must not create a board");

    // The way out the message offers has to be one that actually works -- see the marker test.
    assert!(ai_kanban::render::error(&err).contains(".ai-kanban"));
}

#[test]
fn directories_above_home_are_refused_too() {
    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("alice");
    fs::create_dir_all(&home).unwrap();
    let s = store();

    assert!(refused(s.resolve_project_from(parent.path(), Some(&home))));
    assert!(refused(s.resolve_project_from(Path::new("/"), Some(&home))));
    assert!(s.all_projects().unwrap().is_empty());
}

#[test]
fn projects_under_home_each_keep_their_own_board() {
    // The damage replayed. A session in $HOME comes first; two unrelated non-git projects
    // beneath it follow. They must end up on two boards, not on one named after the user.
    let home = tempfile::tempdir().unwrap();
    let docs = home.path().join("Documents").join("wow-docs");
    let app = home.path().join("Documents").join("md2pdf");
    fs::create_dir_all(&docs).unwrap();
    fs::create_dir_all(&app).unwrap();
    let s = store();

    assert!(refused(s.resolve_project_from(home.path(), Some(home.path()))));
    let a = s.resolve_project_from(&docs, Some(home.path())).unwrap();
    let b = s.resolve_project_from(&app, Some(home.path())).unwrap();

    assert_eq!(a.how, Resolution::Directory);
    assert_eq!(a.project.name, "wow-docs");
    assert_eq!(b.project.name, "md2pdf");
    assert_ne!(a.project.id, b.project.id);
}

#[test]
fn a_sibling_that_only_shares_a_prefix_with_home_is_not_refused() {
    // `/Users/alice-old` is not above `/Users/alice`. Paths compare by component, not by string.
    let parent = tempfile::tempdir().unwrap();
    let home = parent.path().join("alice");
    let sibling = parent.path().join("alice-old");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    let s = store();

    assert!(s.resolve_project_from(&sibling, Some(&home)).unwrap().created);
}

#[test]
fn a_marker_in_home_still_makes_it_a_board() {
    // The escape hatch the refusal names. Only the fallback is refused; an explicit marker
    // says the directory was meant.
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(".ai-kanban"), "dotfiles\n").unwrap();
    let s = store();

    let r = s.resolve_project_from(home.path(), Some(home.path())).unwrap();
    assert_eq!(r.how, Resolution::Marker);
    assert_eq!(r.project.key, "dotfiles");
}

#[test]
fn a_git_repo_in_home_still_makes_it_a_board() {
    // A dotfiles checkout in $HOME is a real project with a real anchor.
    let home = tempfile::tempdir().unwrap();
    fake_repo(home.path(), Some("git@github.com:me/dotfiles.git"));
    let s = store();

    let r = s.resolve_project_from(home.path(), Some(home.path())).unwrap();
    assert_eq!(r.how, Resolution::GitRemote);
}

#[test]
fn a_board_that_already_claims_home_still_resolves() {
    // Stores from before the refusal can already have one. Refusing it too would leave
    // that board unreadable from the place it lives, which is where someone repairs it.
    let home = tempfile::tempdir().unwrap();
    let elsewhere = home.path().join("legacy");
    fs::create_dir_all(&elsewhere).unwrap();
    let s = store();
    let pid = s.resolve_project_from(&elsewhere, Some(home.path())).unwrap().project.id;
    s.add_path_alias(pid, &fs::canonicalize(home.path()).unwrap()).unwrap();

    let r = s.resolve_project_from(home.path(), Some(home.path())).unwrap();
    assert_eq!(r.how, Resolution::KnownPath);
    assert_eq!(r.project.id, pid);
}

#[test]
fn the_temp_directory_itself_is_refused_but_directories_in_it_are_not() {
    let s = store();
    assert!(refused(s.resolve_project_from(&std::env::temp_dir(), None)));

    // Every other test in this suite resolves a tempdir inside it.
    let inside = tempfile::tempdir().unwrap();
    assert!(s.resolve_project_from(inside.path(), None).is_ok());
}

#[test]
fn a_directory_that_becomes_a_git_repo_later_keeps_its_board() {
    // Pinned on purpose, because it looks like a bug worth fixing. A plain directory gets a
    // board, then `git init` runs in it. The learned alias still answers first, so the
    // directory stays on its board. Letting the new `.git` win would re-key the directory
    // out from under every task already filed there.
    let tmp = tempfile::tempdir().unwrap();
    let s = store();

    let before = s.resolve_project(tmp.path()).unwrap();
    assert_eq!(before.how, Resolution::Directory);

    fake_repo(tmp.path(), Some("git@github.com:me/later.git"));
    let after = s.resolve_project(tmp.path()).unwrap();

    assert_eq!(after.how, Resolution::KnownPath);
    assert_eq!(after.project.id, before.project.id);
}

/// A linked worktree of `main` at `wt`, laid out the way `git worktree add` leaves it: a
/// `.git` file pointing at `<main>/.git/worktrees/<name>`, whose `commondir` leads back.
fn fake_worktree(main: &Path, wt: &Path, name: &str) {
    let gitdir = main.join(".git").join("worktrees").join(name);
    fs::create_dir_all(&gitdir).unwrap();
    fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    fs::create_dir_all(wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
}

#[test]
fn a_worktree_of_a_repo_without_a_remote_joins_the_main_board() {
    // With no remote, both sides used to key on their own directory -- `path:<main>` and
    // `path:<worktree>` -- so every worktree started an empty board.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    let wt = tmp.path().join("repo-feature");
    fs::create_dir_all(&main).unwrap();
    fake_repo(&main, None);
    fake_worktree(&main, &wt, "repo-feature");
    let s = store();

    let m = s.resolve_project(&main).unwrap();
    let w = s.resolve_project(&wt).unwrap();

    assert_eq!(w.project.id, m.project.id, "worktree must not fork the board");
    assert_eq!(w.how, Resolution::KnownPath);
    assert!(!w.created);
}

#[test]
fn a_worktree_used_before_its_main_checkout_still_lands_on_the_same_board() {
    // Order must not matter. The worktree derives its identity from the main checkout's
    // root, so a later session in the main checkout arrives at the same key.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    let wt = tmp.path().join("elsewhere").join("feature");
    fs::create_dir_all(&main).unwrap();
    fake_repo(&main, None);
    fake_worktree(&main, &wt, "feature");
    let s = store();

    let w = s.resolve_project(&wt).unwrap();
    assert!(w.created);
    assert_eq!(w.project.name, "repo", "named after the repo, not the worktree directory");
    assert_eq!(w.project.key, format!("path:{}", fs::canonicalize(&main).unwrap().display()));

    let m = s.resolve_project(&main).unwrap();
    assert_eq!(m.project.id, w.project.id);
    assert!(!m.created);
}

#[test]
fn a_worktree_joins_a_board_keyed_before_the_repo_had_a_remote() {
    // ai-kanban's own situation: its board was created as `path:` while the repo had no
    // remote. Once `origin` exists, a worktree derives `git:<remote>` -- a key no board
    // has -- unless it resolves as the main checkout, whose path the board already claims.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("repo");
    let wt = tmp.path().join("repo-wt");
    fs::create_dir_all(&main).unwrap();
    fake_repo(&main, None);
    let s = store();
    let before = s.resolve_project(&main).unwrap();
    assert_eq!(before.how, Resolution::GitRoot);

    fake_repo(&main, Some("git@github.com:me/repo.git"));
    fake_worktree(&main, &wt, "repo-wt");
    let w = s.resolve_project(&wt).unwrap();

    assert_eq!(w.project.id, before.project.id, "a remote added later must not fork worktrees off");
    assert_eq!(s.find_project(&wt).unwrap().unwrap().id, before.project.id, "the read-only lookup must agree");
}

#[test]
fn a_submodule_keeps_its_own_board() {
    // A submodule's `.git` is also a file, but its gitdir has no `commondir` (verified with
    // a real `git submodule add`): it is a separate repository, not another checkout of the
    // superproject.
    let tmp = tempfile::tempdir().unwrap();
    let sup = tmp.path().join("super");
    fs::create_dir_all(&sup).unwrap();
    fake_repo(&sup, Some("git@github.com:me/super.git"));
    let modgit = sup.join(".git").join("modules").join("lib");
    fs::create_dir_all(&modgit).unwrap();
    fs::write(modgit.join("config"), "[remote \"origin\"]\n\turl = git@github.com:me/lib.git\n").unwrap();
    let sub = sup.join("lib");
    fs::create_dir_all(&sub).unwrap();
    // Relative, as `git submodule add` writes it (checked against git 2.55).
    fs::write(sub.join(".git"), "gitdir: ../.git/modules/lib\n").unwrap();
    let s = store();

    let parent = s.resolve_project(&sup).unwrap();
    let child = s.resolve_project(&sub).unwrap();

    assert_ne!(child.project.id, parent.project.id);
    assert_eq!(child.project.key, "git:github.com/me/lib");
}

#[test]
fn a_worktree_honours_a_marker_that_exists_only_in_the_main_checkout() {
    // An untracked `.ai-kanban` is never copied into a worktree. The worktree has to continue
    // its walk from the same place in the main checkout, not from its root, or the package
    // lands on the repo-wide board in the worktree and on its own board in the main checkout.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("monorepo");
    let wt = tmp.path().join("monorepo-wt");
    fs::create_dir_all(main.join("packages").join("api")).unwrap();
    fake_repo(&main, Some("git@github.com:me/monorepo.git"));
    fs::write(main.join("packages").join("api").join(".ai-kanban"), "monorepo/api\n").unwrap();
    fake_worktree(&main, &wt, "monorepo-wt");
    let deep = wt.join("packages").join("api").join("src");
    fs::create_dir_all(&deep).unwrap();
    let s = store();

    let in_main = s.resolve_project(&main.join("packages").join("api")).unwrap();
    let in_wt = s.resolve_project(&deep).unwrap();

    assert_eq!(in_main.project.key, "monorepo/api");
    assert_eq!(in_wt.project.id, in_main.project.id);
}

#[test]
fn a_worktree_of_a_repo_on_a_named_board_lands_on_that_board() {
    // A registered repo resolves through its path alias on the named board. A worktree
    // must follow it there rather than keying on its own directory and starting a board.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("api");
    let wt = tmp.path().join("api-feature");
    fs::create_dir_all(&main).unwrap();
    fake_repo(&main, None);
    fake_worktree(&main, &wt, "api-feature");
    let s = store();
    let board = s.create_board("BMUKN").unwrap();
    s.add_repo(board.id, &main, None, Actor::User).unwrap();
    let boards = s.all_projects().unwrap().len();

    let w = s.resolve_project(&wt).unwrap();

    assert_eq!(w.project.id, board.id, "worktree must land on the repo's board");
    assert!(!w.created);
    assert_eq!(s.all_projects().unwrap().len(), boards, "a worktree created a board");
}

#[test]
fn a_worktree_of_a_shared_repo_lands_on_its_home_board() {
    // A shared repo is on two boards but opens on one, its home. Its worktree has to agree,
    // or the same checkout reads a different memory depending on which directory it is in.
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("lib");
    let wt = tmp.path().join("lib-feature");
    fs::create_dir_all(&main).unwrap();
    fake_repo(&main, None);
    fake_worktree(&main, &wt, "lib-feature");
    let s = store();
    let home = s.create_board("OTHER").unwrap();
    let guest = s.create_board("BMUKN").unwrap();
    s.add_repo(home.id, &main, None, Actor::User).unwrap();
    s.add_repo(guest.id, &main, None, Actor::User).unwrap();

    let w = s.resolve_project(&wt).unwrap();

    assert_eq!(w.project.id, home.id, "worktree must land on the home board");
    assert!(!w.created);
}
