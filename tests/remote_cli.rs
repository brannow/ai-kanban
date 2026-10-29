//! `ai-kanban remote` and `ai-kanban room`, run as the real binary against a throwaway store and
//! a real relay on a loopback port. Without a terminal, as in a script: every question needs its
//! flag, and a missing one fails before anything changes.

mod common;
use common::relay_http::HttpRelay;

use ai_kanban::core::model::*;
use ai_kanban::core::task::TaskDraft;
use ai_kanban::core::Store;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Machine {
    dir: tempfile::TempDir,
    db: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("kanban.db");
        Machine { dir, db }
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ai-kanban"));
        c.args(args)
            .current_dir(self.dir.path())
            .env("AI_KANBAN_DB", &self.db)
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("AI_KANBAN_SERVER_URL")
            .env_remove("AI_KANBAN_SERVER_KEY")
            .env_remove("AI_KANBAN_SYNC")
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?} failed:\n{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} should have failed:\n{}", String::from_utf8_lossy(&out.stdout));
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    fn store(&self) -> Store {
        Store::open(&self.db).unwrap()
    }

    fn board(&self, name: &str) -> i64 {
        self.store().create_board(name).unwrap().id
    }
}

#[test]
fn setting_the_remote_checks_the_key_takes_the_tag_and_never_prints_the_key() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    assert!(m.ok(&["remote"]).contains("No remote set"));

    let bad = m.fails(&["remote", &relay.url, "--key", "akb_wrong"]);
    assert!(bad.contains("rejected the key"), "{bad}");
    assert!(!m.dir.path().join("remote.json").exists(), "nothing is saved for a key that does not work");

    let key = relay.key("benny", "b1");
    let out = m.ok(&["remote", &relay.url, "--key", &key]);
    assert!(out.contains("logged in as benny, origin tag b1"), "{out}");
    assert_eq!(m.store().origin_tag().unwrap().as_deref(), Some("b1"));

    let file = m.dir.path().join("remote.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let shown = m.ok(&["remote"]);
    assert!(shown.contains(&relay.url) && shown.contains("benny") && shown.contains("tag b1"), "{shown}");
    assert!(!shown.contains(&key), "the key is masked: {shown}");
}

#[test]
fn connecting_needs_a_board_and_a_yes_without_a_terminal_then_publishes_and_backs_up() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    let pid = m.board("sbb");
    m.store().create_task(pid, TaskDraft::new("already here")).unwrap();
    m.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    assert!(m.ok(&["room", "list"]).contains("No rooms"));

    let no_board = m.fails(&["room", "connect", "sbahn"]);
    assert!(no_board.contains("--board") && no_board.contains("sbb"), "{no_board}");
    let no_yes = m.fails(&["room", "connect", "sbahn", "--board", "sbb"]);
    assert!(no_yes.contains("does not exist yet") && no_yes.contains("--yes"), "{no_yes}");
    assert_eq!(m.store().room_of(pid).unwrap(), None, "nothing changed without the confirmation");

    let out = m.ok(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    assert!(out.contains("will publish:  1 tasks") && out.contains("connected."), "{out}");
    assert_eq!(m.store().room_of(pid).unwrap().as_deref(), Some("sbahn"));
    let backups: Vec<_> = std::fs::read_dir(m.dir.path()).unwrap()
        .filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("kanban-before-connect-sbahn-")).collect();
    assert_eq!(backups.len(), 1, "a backup is written before anything leaves the machine");

    let list = m.ok(&["room", "list"]);
    assert!(list.contains("sbahn") && list.contains("<- your board sbb") && list.contains("b1 benny"), "{list}");
    let status = m.ok(&["room", "status"]);
    assert!(status.contains("sbb @sbahn") && status.contains("unsent      0") && status.contains("team        b1 benny"), "{status}");

    let again = m.fails(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    assert!(again.contains("already connected"), "{again}");
}

/// Being certain it is the right room before connecting: what is in it can be read without
/// joining, and a board can be made for it on the spot -- but never by mistyping a name.
#[test]
fn a_room_can_be_looked_into_without_joining_and_connected_to_a_new_board() {
    let relay = HttpRelay::start();
    let (benny, volker) = (Machine::new(), Machine::new());
    let bp = benny.board("sbb");
    benny.store().create_task(bp, TaskDraft::new("Timetable import fails on DST")).unwrap();
    benny.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    benny.ok(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    volker.ok(&["remote", &relay.url, "--key", &relay.key("volker", "v1")]);

    let list = volker.ok(&["room", "list"]);
    assert!(list.contains("from board \"sbb\" by benny") && list.contains("b1 benny"), "{list}");
    let show = volker.ok(&["room", "show", "sbahn"]);
    assert!(show.contains("created by benny from board \"sbb\"") && show.contains("Timetable import fails on DST"), "{show}");
    assert!(show.contains("#1-b1"), "the handles the room's members use: {show}");
    assert!(!volker.ok(&["room", "list"]).contains("v1 volker"), "looking is not joining");

    let no_board = volker.fails(&["room", "connect", "sbahn"]);
    assert!(no_board.contains("--new-board"), "without a terminal it says how to make one: {no_board}");
    volker.fails(&["room", "connect", "sbahn", "--board", "sbhan", "--yes"]);
    assert!(volker.store().all_projects().unwrap().is_empty(), "a mistyped --board creates nothing");
    let both = volker.fails(&["room", "connect", "sbahn", "--board", "x", "--new-board", "--yes"]);
    assert!(both.contains("contradict"), "{both}");

    let out = volker.ok(&["room", "connect", "sbahn", "--new-board", "--yes"]);
    assert!(out.contains("a new board \"sbahn\"") && out.contains("repo add"), "and how to give it a folder: {out}");
    let vp = volker.store().project_by_name_or_key("sbahn").unwrap().id;
    let got: Vec<String> = volker.store().board(vp, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert_eq!(got, ["Timetable import fails on DST"]);
}

#[test]
fn two_machines_share_a_board_through_the_cli() {
    let relay = HttpRelay::start();
    let (benny, volker) = (Machine::new(), Machine::new());
    let bp = benny.board("sbb");
    let vp = volker.board("sbahn upgrade");
    benny.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    volker.ok(&["remote", &relay.url, "--key", &relay.key("volker", "v1")]);
    benny.ok(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    let out = volker.ok(&["room", "connect", "sbahn", "--board", "sbahn upgrade", "--yes"]);
    assert!(out.contains("from benny (b1)"), "the summary names who is already there: {out}");

    let t = benny.store().create_task(bp, TaskDraft::new("found by benny")).unwrap();
    benny.ok(&["room", "sync"]);
    volker.ok(&["room", "sync"]);
    let got: Vec<String> = volker.store().board(vp, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert_eq!(got, ["found by benny"]);
    let rendered = ai_kanban::render::board(&volker.store().board(vp, &BoardQuery::board()).unwrap());
    assert!(rendered.contains(&format!("#{}-b1", t.id)), "{rendered}");
}

#[test]
fn disconnecting_asks_about_unsent_changes_and_foreign_tasks_first() {
    let relay = HttpRelay::start();
    let (benny, volker) = (Machine::new(), Machine::new());
    let bp = benny.board("sbb");
    volker.board("x");
    benny.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    volker.ok(&["remote", &relay.url, "--key", &relay.key("volker", "v1")]);
    benny.ok(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    volker.ok(&["room", "connect", "sbahn", "--board", "x", "--yes"]);
    let vp = volker.store().board_of_room("sbahn").unwrap().unwrap();
    volker.store().create_task(vp, TaskDraft::new("volker's")).unwrap();
    volker.ok(&["room", "sync"]);
    benny.ok(&["room", "sync"]);
    benny.store().create_task(bp, TaskDraft::new("unsent")).unwrap();

    let unsent = benny.fails(&["room", "disconnect", "sbahn", "--yes"]);
    assert!(unsent.contains("not pushed yet") && unsent.contains("--sync-first or --discard"), "{unsent}");
    let foreign = benny.fails(&["room", "disconnect", "sbahn", "--sync-first", "--yes"]);
    assert!(foreign.contains("came from other members") && foreign.contains("--keep-foreign or --wipe-foreign"), "{foreign}");
    assert!(benny.store().room_of(bp).unwrap().is_some(), "still connected after the refusals");

    let out = benny.ok(&["room", "disconnect", "sbahn", "--sync-first", "--keep-foreign", "--yes"]);
    assert!(out.contains("is local again"), "{out}");
    assert_eq!(benny.store().room_of(bp).unwrap(), None);
    volker.ok(&["room", "sync"]);
    let v: Vec<String> = volker.store().board(vp, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert!(v.contains(&"unsent".to_string()), "synced before uncoupling: {v:?}");
    let kept: Vec<String> = benny.store().board(bp, &BoardQuery::board()).unwrap().tasks.into_iter().map(|t| t.title).collect();
    assert!(kept.contains(&"volker's".to_string()), "kept: {kept:?}");
}

#[test]
fn removing_the_remote_uncouples_every_board_and_forgets_the_key() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    m.board("a");
    m.board("b");
    m.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    m.ok(&["room", "connect", "one", "--board", "a", "--yes"]);
    m.ok(&["room", "connect", "two", "--board", "b", "--yes"]);
    let refused = m.fails(&["remote", "--disconnect"]);
    assert!(refused.contains("--yes"), "{refused}");
    m.ok(&["remote", "--disconnect", "--yes", "--keep-foreign", "--discard"]);
    assert!(m.store().connected_boards().unwrap().is_empty());
    assert!(!m.dir.path().join("remote.json").exists());
    assert!(m.ok(&["remote"]).contains("No remote set"));
}

#[test]
fn a_new_relay_url_is_refused_while_boards_are_connected_but_a_move_is_fine() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    m.board("a");
    let key = relay.key("benny", "b1");
    m.ok(&["remote", &relay.url, "--key", &key]);
    m.ok(&["room", "connect", "one", "--board", "a", "--yes"]);
    let other = HttpRelay::start();
    let refused = m.fails(&["remote", &other.url, "--key", &other.key("benny", "b1")]);
    assert!(refused.contains("Disconnect them first") && refused.contains("--move"), "{refused}");

    // The same relay under another name -- here, localhost for 127.0.0.1.
    let moved = relay.url.replace("127.0.0.1", "localhost");
    assert!(m.ok(&["remote", "--move", &moved]).contains("now reached at"));
    assert!(m.ok(&["room", "sync"]).contains("one: pushed"));
    let not_a_move = m.fails(&["remote", "--move", &other.url]);
    assert!(not_a_move.contains("rejected the key") || not_a_move.contains("different relay"), "{not_a_move}");
}

#[test]
fn sync_off_for_a_run_keeps_it_local_and_says_so() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    m.board("a");
    m.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    let off = [("AI_KANBAN_SYNC", "off")];
    let out = m.run_env(&["room", "list"], &off);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("AI_KANBAN_SYNC is off"));
    let shown = m.run_env(&["remote"], &off);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("off for this run"));
}
