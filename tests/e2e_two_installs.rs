//! End to end: two installations of ai-kanban share one board through a relay.
//!
//! Each installation is one store -- `install-a/test-a.db` (benny, tag a1) and
//! `install-b/test-b.db` (volker, tag b1) -- with its own `remote.json` beside it. Everything goes
//! through the real `ai-kanban` binary: setup through the CLI, the way a person does it, and the
//! work through `ai-kanban mcp` over stdio JSON-RPC, the way Claude Code drives it. So the
//! background syncer, the notification stream and the catch-up before a claim all run for real,
//! in separate processes, exactly as two people's sessions would.
//!
//! The relay runs in-process (the same code the container runs). Every step that crosses to the
//! other installation waits for it with a bound, so a broken sync fails instead of hanging.

mod common;
use common::relay_http::HttpRelay;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const ROOM: &str = "sbahn";
/// Long enough for a notification, a backoff after an outage and a round; short enough that a
/// sync that never arrives fails the test in reasonable time.
const CROSS: Duration = Duration::from_secs(40);

/// One installation: a store, its remote config, and the binary pointed at them.
struct Install {
    dir: PathBuf,
    db: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Install {
    fn new(name: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(format!("install-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join(format!("test-{name}.db"));
        Install { dir, db, _tmp: tmp }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ai-kanban"));
        c.current_dir(&self.dir)
            .env("AI_KANBAN_DB", &self.db)
            .env("CLAUDE_PROJECT_DIR", &self.dir)
            .env_remove("AI_KANBAN_SERVER_URL")
            .env_remove("AI_KANBAN_SERVER_KEY")
            .env_remove("AI_KANBAN_SYNC");
        c
    }

    fn cli(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).stdin(Stdio::null()).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{args:?} failed:\n{text}");
        text
    }

    /// The local id a title has in this store -- what only this machine knows.
    fn local_id(&self, title: &str) -> i64 {
        let s = ai_kanban::core::Store::open(&self.db).unwrap();
        s.conn.query_row("SELECT id FROM tasks WHERE title = ?1", [title], |r| r.get(0)).unwrap()
    }

    fn agent(&self) -> Agent {
        Agent::start(self.cmd())
    }
}

/// An `ai-kanban mcp` process, driven over stdio like Claude Code drives it.
struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Agent {
    fn start(mut cmd: Command) -> Self {
        let mut child = cmd.arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut a = Agent { child, stdin, lines, next_id: 1 };
        let init = a.request("initialize", json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "e2e", "version": "0" },
        }));
        assert!(init.get("result").is_some(), "initialize failed: {init}");
        a.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        a
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).unwrap_or_else(|_| panic!("no answer to {method}"));
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
        }
    }

    /// A tool call: its text on success, `Err` with the message the agent would read otherwise.
    fn try_tool(&mut self, name: &str, args: Value) -> Result<String, String> {
        let v = self.request("tools/call", json!({ "name": name, "arguments": args }));
        if let Some(e) = v.get("error") {
            return Err(e["message"].as_str().unwrap_or_default().to_string());
        }
        let text = v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
        if v["result"]["isError"] == json!(true) { Err(text) } else { Ok(text) }
    }

    fn tool(&mut self, name: &str, args: Value) -> String {
        self.try_tool(name, args.clone()).unwrap_or_else(|e| panic!("{name} {args} failed: {e}"))
    }

    /// Calls a tool until its answer satisfies `ok`, within `CROSS`.
    fn until(&mut self, what: &str, name: &str, args: Value, ok: impl Fn(&str) -> bool) -> String {
        let start = Instant::now();
        loop {
            let text = self.try_tool(name, args.clone()).unwrap_or_else(|e| e);
            if ok(&text) {
                return text;
            }
            assert!(start.elapsed() < CROSS, "timed out waiting for: {what}\nlast answer:\n{text}");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn board(project: &str) -> Value {
    json!({ "project": project })
}

#[test]
fn two_installations_share_a_board_end_to_end() {
    let mut relay = HttpRelay::start();
    let (a, b) = (Install::new("a"), Install::new("b"));

    // --- Setup, as each person would do it ------------------------------------------------
    a.cli(&["board", "add", "sbb"]);
    a.cli(&["board", "add", "private-a"]);
    b.cli(&["board", "add", "sbahn-upgrade"]);
    assert!(a.cli(&["remote", &relay.url, "--key", &relay.key("benny", "a1")]).contains("origin tag a1"));
    assert!(b.cli(&["remote", &relay.url, "--key", &relay.key("volker", "b1")]).contains("origin tag b1"));
    a.cli(&["room", "connect", ROOM, "--board", "sbb", "--yes"]);
    b.cli(&["room", "connect", ROOM, "--board", "sbahn-upgrade", "--yes"]);
    assert!(a.dir.join("remote.json").exists() && b.dir.join("remote.json").exists());
    assert!(!a.dir.join("test-a.db").to_string_lossy().is_empty() && b.db.ends_with("test-b.db"));

    let (mut aa, mut ba) = (a.agent(), b.agent());

    // --- A files a task; B's session receives it without asking ---------------------------
    let filed = aa.tool("task_add", json!({ "project": "sbb", "title": "Login times out behind the proxy" }));
    assert!(filed.contains("Board: sbb @sbahn"), "the header names the room:\n{filed}");
    let login = a.local_id("Login times out behind the proxy");
    let login_h = format!("{login}-a1");
    assert!(filed.contains(&format!("#{login_h}")), "A sees its own task by its handle:\n{filed}");
    let seen = ba.until("B sees A's task", "board", board("sbahn-upgrade"), |t| t.contains(&format!("#{login_h} ")));
    assert!(seen.contains("Board: sbahn-upgrade @sbahn"), "{seen}");
    ba.until("B's header knows the team", "board", board("sbahn-upgrade"), |t| t.contains("team: a1 benny, b1 volker"));

    // --- B claims it by A's handle; A sees the claim and the reason ------------------------
    ba.tool("task_update", json!({ "project": "sbahn-upgrade", "task": login_h, "status": "doing", "log": "taking it, I know this proxy" }));
    aa.until("A sees B's claim", "task_show", json!({ "project": "sbb", "task": login }), |t| {
        t.contains("doing") && t.contains("taking it, I know this proxy")
    });

    // --- A claims it too: the answer leads with who already has it ---------------------------
    let overlap = aa.tool("task_update", json!({ "project": "sbb", "task": login, "status": "doing" }));
    assert!(overlap.starts_with(&format!("Heads-up: volker has had #{login_h} in doing")), "{overlap}");
    assert!(overlap.contains("by volker"), "the row says who:\n{overlap}");

    // --- B files what A already filed: the answer points at A's task ---------------------------
    let dup = ba.tool("task_add", json!({ "project": "sbahn-upgrade", "title": "Login timeout behind the proxy again" }));
    assert!(dup.starts_with("Possible duplicate of what you just filed:") && dup.contains(&format!("#{login_h} ")), "{dup}");

    // --- B's findings land on A's task: a log naming B's own task, a note ------------------
    ba.tool("task_add", json!({ "project": "sbahn-upgrade", "title": "Raise proxy keep-alive timeout" }));
    let keepalive = b.local_id("Raise proxy keep-alive timeout");
    let keepalive_h = format!("{keepalive}-b1");
    ba.tool("log", json!({ "project": "sbahn-upgrade", "task": login_h, "body": format!("root cause: keep-alive, fix in #{keepalive}") }));
    ba.tool("note_add", json!({ "project": "sbahn-upgrade", "task": login_h, "title": "nginx drops idle upstream connections after 60s", "paths": "deploy/nginx.conf" }));
    let shown = aa.until("A reads B's finding with B's number qualified", "task_show", json!({ "project": "sbb", "task": login }), |t| {
        t.contains(&format!("fix in #{keepalive_h}")) && t.contains("nginx drops idle upstream")
    });
    assert!(!shown.contains(&format!("fix in #{keepalive} ")), "never B's bare number on A's machine:\n{shown}");
    let hits = aa.tool("recall", json!({ "project": "sbb", "query": "idle upstream" }));
    assert!(hits.contains("nginx drops idle upstream"), "A's recall finds B's knowledge:\n{hits}");

    // --- A blocks its task on B's; B's board says so by A's spelling of nothing local -------
    aa.tool("task_update", json!({ "project": "sbb", "task": login, "blocked_by": keepalive_h }));
    ba.until("B sees the blocker by handle", "board", board("sbahn-upgrade"), |t| t.contains(&format!("blocked by #{keepalive_h}")));

    // --- A plain number that is a teammate's hidden local id is refused with the handle -----
    let hidden = a.local_id("Raise proxy keep-alive timeout");
    let refused = aa.try_tool("task_show", json!({ "project": "sbb", "task": hidden })).unwrap_err();
    assert!(refused.contains(&format!("#{keepalive_h}")), "{refused}");

    // --- A private board never reaches the room ---------------------------------------------
    aa.tool("task_add", json!({ "project": "private-a", "title": "secret customer migration plan" }));
    aa.tool("task_add", json!({ "project": "sbb", "title": "flush marker after the private write" }));
    ba.until("B got the marker", "board", board("sbahn-upgrade"), |t| t.contains("flush marker"));
    let none = ba.tool("recall", json!({ "project": "all", "query": "secret customer" }));
    assert!(none.contains("No hits"), "{none}");
    let log = serde_json::to_string(&relay.admin().rooms().unwrap()).unwrap();
    assert!(!log.contains("private-a"), "{log}");

    // --- The web page names tasks the same way ------------------------------------------------
    {
        let s = ai_kanban::core::Store::open(&a.db).unwrap();
        let pid = s.board_of_room(ROOM).unwrap().unwrap();
        let h = s.handles(&[]).unwrap();
        assert_eq!(h.task(hidden), format!("#{keepalive_h}"), "the page's handles map");
        let _ = pid;
    }

    // --- The relay goes away: A keeps working, the header says so, the backlog drains --------
    relay.stop();
    aa.tool("task_add", json!({ "project": "sbb", "title": "written while the relay is down" }));
    aa.until("A's header says offline", "board", board("sbb"), |t| t.contains("offline") && t.contains("unsent"));
    relay.resume();
    aa.tool("log", json!({ "project": "sbb", "body": "relay is back" }));
    ba.until("B gets what A wrote offline", "board", board("sbahn-upgrade"), |t| t.contains("written while the relay is down"));
    aa.until("A's header is clean again", "board", board("sbb"), |t| !t.contains("offline") && !t.contains("unsent"));

    // --- The relay admin forgets a task: it leaves both installations -----------------------
    aa.tool("task_add", json!({ "project": "sbb", "title": "pasted a token: AKIAEXAMPLE" }));
    ba.until("B got the leak", "board", board("sbahn-upgrade"), |t| t.contains("AKIAEXAMPLE"));
    let leak = a.local_id("pasted a token: AKIAEXAMPLE");
    let refused = aa.try_tool("task_update", json!({ "project": "sbb", "task": leak, "status": "archived" }));
    assert!(refused.is_ok(), "archiving is fine; forgetting is the admin's act");
    relay.admin().forget(ROOM, &ai_kanban::sync::protocol::Ref { kind: "task".into(), origin: "a1".into(), id: leak }).unwrap();
    aa.tool("log", json!({ "project": "sbb", "body": "nudge after the forget" }));
    let gone = |t: &str| !t.contains("AKIAEXAMPLE");
    ba.until("B lost the forgotten task", "recall", json!({ "project": "all", "query": "AKIAEXAMPLE" }), |t| gone(t) || t.contains("No hits"));
    aa.until("A lost it too", "recall", json!({ "project": "all", "query": "AKIAEXAMPLE" }), |t| gone(t) || t.contains("No hits"));

    // --- A takes a task private: B keeps an archived copy that says who moved it -------------
    aa.tool("task_add", json!({ "project": "sbb", "title": "turns out to be customer-specific" }));
    let moving = a.local_id("turns out to be customer-specific");
    ba.until("B got it", "board", board("sbahn-upgrade"), |t| t.contains("customer-specific"));
    aa.tool("task_update", json!({ "project": "sbb", "task": moving, "move_to": "private-a", "log": "belongs to one customer" }));
    let copy = ba.until("B's copy is archived", "task_show", json!({ "project": "sbahn-upgrade", "task": format!("{moving}-a1") }), |t| {
        t.contains("archived") && t.contains("benny moved this off the team board")
    });
    assert!(!copy.contains("private-a"), "the private board's name stays private:\n{copy}");

    // --- Status, then B leaves the room ------------------------------------------------------
    drop(aa);
    drop(ba);
    let status = a.cli(&["room", "status"]);
    assert!(status.contains("sbb @sbahn") && status.contains("team        a1 benny, b1 volker"), "{status}");
    let out = b.cli(&["room", "disconnect", ROOM, "--sync-first", "--keep-foreign", "--yes"]);
    assert!(out.contains("is local again"), "{out}");
    let mut ba = b.agent();
    let local = ba.tool("board", board("sbahn-upgrade"));
    assert!(local.starts_with("Board: sbahn-upgrade  ("), "no room any more:\n{local}");
    assert!(local.contains("Login times out"), "and the shared work stayed:\n{local}");
}
