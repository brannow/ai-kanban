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
            // A developer's own plugin settings must not reach the tests.
            .env_remove("AI_KANBAN_PLUGIN_RELAY_URL")
            .env_remove("AI_KANBAN_PLUGIN_RELAY_KEY")
            .env_remove("AI_KANBAN_PLUGIN_RELAY_NAME")
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

/// Keys are issued without a name; the person names themselves when setting the remote, and
/// that first name fixes the tag. Renaming later changes only the name.
#[test]
fn the_first_name_fixes_the_tag_and_a_rename_keeps_it() {
    let relay = HttpRelay::start();
    let (volker, other) = (Machine::new(), Machine::new());
    let (_, key) = relay.admin().issue_key("volker laptop").unwrap();
    let out = volker.ok(&["remote", &relay.url, "--key", &key, "--name", "Volker"]);
    assert!(out.contains("origin tag v1") && out.contains("sees you as Volker"), "{out}");
    assert!(!out.contains(&key), "the key is never printed");

    let runs = relay.admin().keys().unwrap().into_iter().find(|k| k.tag.as_deref() == Some("v1")).unwrap().client;
    assert_eq!(runs, Some(format!("ai-kanban/{} (protocol 1)", env!("CARGO_PKG_VERSION"))), "the relay knows what this machine runs");

    let out = volker.ok(&["remote", "--name", "Volker K."]);
    assert!(out.contains("Volker K. (tag v1)"), "{out}");
    assert_eq!(volker.store().origin_tag().unwrap().as_deref(), Some("v1"));

    // Without a name and without a terminal, the relay gives one.
    let (_, key) = relay.admin().issue_key("someone").unwrap();
    let out = other.ok(&["remote", &relay.url, "--key", &key]);
    assert!(out.contains("sees you as ") && out.contains('-'), "a name like brave-otter: {out}");
}

/// A plugin user names themselves in the settings. That name must reach the relay before
/// anything else, or a generated one would pick the tag.
#[test]
fn the_name_from_the_plugin_settings_picks_the_tag() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    let (_, key) = relay.admin().issue_key("carla desktop").unwrap();
    let out = m.run_env(&["remote", &relay.url, "--key", &key], &[("AI_KANBAN_PLUGIN_RELAY_NAME", "Carla")]);
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.contains("origin tag c1") && out.contains("sees you as Carla"), "{out}");
}

/// The real order for a plugin user: URL, key and name in the settings, a session starts (the
/// MCP server has the settings), and later Claude runs `room connect` in its shell -- which
/// does not. The name has to have reached the relay before that, with no board connected yet.
#[test]
fn the_session_sends_the_settings_name_before_any_board_is_connected() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    let (_, key) = relay.admin().issue_key("volker laptop").unwrap();
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_ai-kanban"))
        .arg("mcp")
        .current_dir(m.dir.path())
        .env("AI_KANBAN_DB", &m.db)
        .env_remove("AI_KANBAN_SYNC")
        .env("AI_KANBAN_PLUGIN_RELAY_URL", &relay.url)
        .env("AI_KANBAN_PLUGIN_RELAY_KEY", &key)
        .env("AI_KANBAN_PLUGIN_RELAY_NAME", "Volker")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let named = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        relay.admin().keys().unwrap().iter().any(|k| k.tag.as_deref() == Some("v1") && k.name.as_deref() == Some("Volker"))
    });
    mcp.kill().unwrap();
    let _ = mcp.wait();
    assert!(named, "the session named the key from the settings: {:?}", relay.admin().keys().unwrap());

    let out = m.ok(&["room", "connect", "sbahn", "--new-board", "--yes"]);
    assert!(out.contains("connected."), "{out}");
    assert_eq!(m.store().origin_tag().unwrap().as_deref(), Some("v1"), "the shell's connect did not pick another tag");
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

/// Connecting a room to a board refuses what would leave that board silently incomplete,
/// and says the way out: the room's tasks still on another board here, or this machine's own
/// tasks in the room from a board it no longer has.
#[test]
fn connecting_refuses_a_board_that_would_miss_the_rooms_tasks() {
    let relay = HttpRelay::start();
    let m = Machine::new();
    let first = m.board("sbb");
    m.store().create_task(first, TaskDraft::new("filed on sbb")).unwrap();
    m.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    m.ok(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    m.ok(&["room", "disconnect", "sbahn", "--keep-foreign", "--yes"]);

    m.board("second");
    let out = m.fails(&["room", "connect", "sbahn", "--board", "second", "--yes"]);
    assert!(out.contains("on board \"sbb\"") && out.contains("ai-kanban merge \"second\" \"sbb\""), "{out}");
    assert!(m.store().room_of(m.store().project_by_name_or_key("second").unwrap().id).unwrap().is_none(), "nothing connected");
    assert!(!relay.admin().rooms().unwrap()[0].members.iter().any(|x| x.tag == "b1"), "not even joined on the relay");

    m.ok(&["board", "forget", "sbb", "--yes"]);
    let out = m.fails(&["room", "connect", "sbahn", "--new-board", "--yes"]);
    assert!(out.contains("from a board it no longer has") && out.contains("key of its own"), "{out}");
    assert!(m.store().project_by_name_or_key("sbahn").is_err(), "no stray board made");
}

/// The web UI syncs by itself, both ways: what is done on the page goes out within seconds,
/// and what a teammate does shows up on it -- with no agent session running.
#[test]
fn the_web_ui_syncs_both_ways_without_a_session() {
    let relay = HttpRelay::start();
    let (benny, volker) = (Machine::new(), Machine::new());
    let bp = benny.board("sbb");
    volker.board("x");
    benny.ok(&["remote", &relay.url, "--key", &relay.key("benny", "b1")]);
    volker.ok(&["remote", &relay.url, "--key", &relay.key("volker", "v1")]);
    benny.ok(&["room", "connect", "sbahn", "--board", "sbb", "--yes"]);
    volker.ok(&["room", "connect", "sbahn", "--board", "x", "--yes"]);

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut serve = Command::new(env!("CARGO_BIN_EXE_ai-kanban"))
        .args(["serve", "--port", &port.to_string()])
        .current_dir(benny.dir.path())
        .env("AI_KANBAN_DB", &benny.db)
        .env_remove("AI_KANBAN_SYNC")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let until = |what: &str, ok: &dyn Fn() -> bool| {
        let done = (0..100).any(|_| { std::thread::sleep(std::time::Duration::from_millis(100)); ok() });
        assert!(done, "{what} within 10s");
    };
    until("the page answers", &|| ureq::get(&base).call().is_ok());

    // Out: filed on the page, in the room.
    ureq::post(format!("{base}/api/projects/{bp}/tasks"))
        .header("content-type", "application/json")
        .send(r#"{"title": "filed on the page", "origin": "user", "actor": "user"}"#)
        .unwrap();
    // The relay keeps each change as sent; the title is in one of them.
    let in_room = || {
        let n: i64 = relay.admin().conn
            .query_row("SELECT COUNT(*) FROM changes WHERE body LIKE '%filed on the page%'", [], |r| r.get(0))
            .unwrap();
        n > 0
    };
    until("the page's task reaches the room", &in_room);

    // In: a teammate's task, on the page.
    let vp = volker.store().project_by_name_or_key("x").unwrap().id;
    volker.store().create_task(vp, TaskDraft::new("from volker")).unwrap();
    volker.ok(&["room", "sync"]);
    until("volker's task shows on the page", &|| {
        let mut res = ureq::get(format!("{base}/api/projects/{bp}/board")).call().unwrap();
        res.body_mut().read_to_string().unwrap().contains("from volker")
    });
    let _ = serve.kill();
    let _ = serve.wait();
}
