//! Tests for the failure this project cannot afford: one project resolving to two
//! identities, splitting memory into two half-boards with nothing to signal it.
//!
//! These build git fixtures by writing `.git/config` directly rather than shelling out to
//! `git`. That keeps the tests fast and hermetic, and it exercises the exact parsing the
//! resolver does.

use ai_kanban::core::project::{normalize_remote, Resolution};
use ai_kanban::core::Store;
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
