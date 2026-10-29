//! `ai-kanban board` and `ai-kanban repo`, run as the real binary against a throwaway store.
//!
//! These are the person's admin path. The web UI only looks at the board, and the agent never
//! creates or destroys boards, so a command broken here leaves no other way to do the job.
//! Run as a process rather than through the store, because what can break is the part only
//! the binary has: argument parsing, the `--board` and `--yes` flags, and exit codes.

use ai_kanban::core::Store;
use std::path::Path;
use std::process::{Command, Output};

struct Cli {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    root: std::path::PathBuf,
}

impl Cli {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("kanban.db");
        // Canonical, because repo paths are stored canonical: macOS temp dirs live behind a
        // /var -> /private/var symlink.
        let root = std::fs::canonicalize(dir.path()).unwrap();
        Cli { _dir: dir, db, root }
    }

    fn checkout(&self, name: &str) -> std::path::PathBuf {
        let p = self.root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ai-kanban"))
            .args(args)
            .current_dir(cwd)
            .env("AI_KANBAN_DB", &self.db)
            // The binary resolves a board from this before the cwd; one inherited from the
            // session running the tests would point every command at that session's project.
            .env_remove("CLAUDE_PROJECT_DIR")
            .output()
            .unwrap()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.run(cwd, args);
        assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn store(&self) -> Store {
        Store::open(&self.db).unwrap()
    }
}

#[test]
fn a_named_board_gets_repos_and_its_folders_open_on_it() {
    let c = Cli::new();
    c.ok(&c.root, &["board", "add", "BMUKN"]);
    let dup = c.run(&c.root, &["board", "add", "bmukn"]);
    assert!(!dup.status.success(), "a name another board has is refused");

    let api = c.checkout("eee-api");
    let out = c.ok(&c.root, &["repo", "add", api.to_str().unwrap(), "--board", "BMUKN"]);
    assert!(out.contains("board BMUKN"), "{out}");

    let s = c.store();
    let p = s.find_project(&api).unwrap().expect("the folder is claimed");
    assert_eq!(p.name, "BMUKN", "--board put it on the named board, not the cwd's");
}

#[test]
fn a_repo_can_be_renamed_and_its_home_moved() {
    let c = Cli::new();
    c.ok(&c.root, &["board", "add", "HOME"]);
    c.ok(&c.root, &["board", "add", "GUEST"]);
    let lib = c.checkout("lib");
    c.ok(&c.root, &["repo", "add", lib.to_str().unwrap(), "--board", "HOME"]);
    c.ok(&c.root, &["repo", "add", lib.to_str().unwrap(), "--board", "GUEST"]);

    c.ok(&c.root, &["repo", "rename", "lib", "Shared Lib", "--board", "HOME"]);
    let out = c.ok(&c.root, &["repo", "home", "shared-lib", "GUEST"]);
    assert!(out.contains("opens on board GUEST"), "{out}");

    let s = c.store();
    assert_eq!(s.find_project(&lib).unwrap().unwrap().name, "GUEST");
    assert_eq!(s.find_repo("shared-lib").unwrap().name, "shared-lib");
}

#[test]
fn forgetting_is_a_dry_run_until_confirmed() {
    let c = Cli::new();
    c.ok(&c.root, &["board", "add", "DOOMED"]);
    let web = c.checkout("web");
    c.ok(&c.root, &["repo", "add", web.to_str().unwrap(), "--board", "DOOMED"]);

    let dry = c.run(&c.root, &["repo", "forget", "web"]);
    assert!(!dry.status.success(), "a dry run exits non-zero, so a script cannot mistake it for done");
    assert!(String::from_utf8_lossy(&dry.stdout).contains("--yes"));
    assert_eq!(c.store().all_repos().unwrap().len(), 1, "the dry run changed nothing");
    c.ok(&c.root, &["repo", "forget", "web", "--yes"]);
    assert!(c.store().all_repos().unwrap().is_empty());

    let dry = c.run(&c.root, &["board", "forget", "DOOMED"]);
    assert!(!dry.status.success());
    assert_eq!(c.store().all_projects().unwrap().len(), 1, "the dry run changed nothing");
    c.ok(&c.root, &["board", "forget", "DOOMED", "--yes"]);
    assert!(c.store().all_projects().unwrap().is_empty());
}

#[test]
fn an_unknown_repo_names_the_ones_that_exist() {
    let c = Cli::new();
    c.ok(&c.root, &["board", "add", "B"]);
    let api = c.checkout("api");
    c.ok(&c.root, &["repo", "add", api.to_str().unwrap(), "--board", "B"]);
    let out = c.run(&c.root, &["repo", "home", "apii", "B"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("api"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// `ai-kanban ... | head -1`: the reader goes away early. The command must end quietly, not panic
/// with "failed printing to stdout: Broken pipe" (#1066).
#[test]
fn a_command_whose_reader_goes_away_ends_quietly() {
    let c = Cli::new();
    for args in [&["--help"][..], &["projects"][..]] {
        // The reader is gone before the command starts, so its very first write meets a closed
        // pipe -- deterministic, unlike racing a real `head`.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let out = Command::new(env!("CARGO_BIN_EXE_ai-kanban"))
            .args(args)
            .env("AI_KANBAN_DB", &c.db)
            .env_remove("CLAUDE_PROJECT_DIR")
            .stdout(writer)
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked") && !err.contains("Broken pipe"), "{args:?}: {err}");
    }
}

/// One way to say each thing: `repo rm --all` is what `repo forget` was, `board list` and
/// `board merge` what `projects` and `merge` were. The older spellings keep working -- boards'
/// notes and logs, the agents' memory, name them -- but the help lists only the new ones.
#[test]
fn the_tidied_commands_work_and_the_older_spellings_still_do() {
    let c = Cli::new();
    c.ok(&c.root, &["board", "add", "ONE"]);
    c.ok(&c.root, &["board", "add", "TWO"]);
    let lib = c.checkout("lib");
    c.ok(&c.root, &["repo", "add", lib.to_str().unwrap(), "--board", "ONE"]);
    c.ok(&c.root, &["repo", "add", lib.to_str().unwrap(), "--board", "TWO"]);

    let dry = c.run(&c.root, &["repo", "rm", "lib", "--all"]);
    assert!(!dry.status.success() && String::from_utf8_lossy(&dry.stdout).contains("off 2 boards"), "{}", String::from_utf8_lossy(&dry.stdout));
    assert_eq!(c.store().all_repos().unwrap().len(), 1, "a dry run");
    assert!(c.ok(&c.root, &["repo", "rm", "lib", "--all", "--yes"]).contains("from every board"));
    assert!(c.store().all_repos().unwrap().is_empty());

    assert_eq!(c.ok(&c.root, &["board", "list"]), c.ok(&c.root, &["projects"]));
    assert!(c.ok(&c.root, &["board", "merge", "ONE", "TWO"]).contains("Moved"));
    assert_eq!(c.store().all_projects().unwrap().len(), 1);
    c.ok(&c.root, &["board", "add", "THREE"]);
    c.ok(&c.root, &["merge", "ONE", "THREE"]);
    assert_eq!(c.store().all_projects().unwrap().len(), 1, "`merge` still works");

    let help = c.ok(&c.root, &["--help"]);
    for gone in ["repo forget", "repo home", "ai-kanban projects", "ai-kanban merge", "room status"] {
        assert!(!help.contains(gone), "{gone} is not advertised any more");
    }
    assert!(help.starts_with("ai-kanban ") && help.contains("(protocol "), "the version comes first");
}
