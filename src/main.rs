//! `ai-kanban` -- a single static binary.
//!
//! Subcommands rather than separate binaries because "little setup overhead" is the pitch:
//! one file to install, one file to update, and `serve` (HTTP, for a future web UI) will
//! land here rather than in a second stack.

use ai_kanban::core::Store;
use ai_kanban::hook;
use ai_kanban::mcp::server::AiKanban;
use rmcp::{transport::stdio, ServiceExt};

const USAGE: &str = "\
ai-kanban -- a kanban board an agent can actually use

Boards:
    ai-kanban board list               Every board, and the room it is shared through (@room)
    ai-kanban board add <name>         A board for a project that is not one folder
    ai-kanban board merge <keep> <gone>    Join a board that split into two
    ai-kanban board forget <board>     Delete a board and all its history (dry run without --yes)
    ai-kanban serve [--port N]         The web UI on http://127.0.0.1:7373; syncs shared boards too

Repos -- the checkouts a board's tasks live in:
    ai-kanban repo list [board|all]    Repos on this folder's board, another, or every one
    ai-kanban repo add <path>          Register a checkout on this folder's board (--board <b>)
    ai-kanban repo rm <repo>           Take it off this folder's board (--board <b>), or every
                                       board (--all; dry run without --yes)
    ai-kanban repo rename <repo> <new>

Sharing with a team (docs/sync.md):
    ai-kanban remote <url> --key K     Connect this machine to a relay (--name N: how the team sees you)
    ai-kanban remote                   Show it. --name N renames you; --move <url>, --disconnect
    ai-kanban room list                Rooms on the relay, and how your shared boards are doing
    ai-kanban room show <room>         What is in a room, as a board, without joining it
    ai-kanban room connect <room>      Share a board through a room (--board <b> | --new-board, --yes)
    ai-kanban room disconnect <room>   Uncouple it again

Your data:
    ai-kanban backup <file>            Copy the whole store to one consistent file
    ai-kanban export [board...]        The store as JSON on stdout
    ai-kanban import <file>            Restore boards from an export
    ai-kanban where                    Print the path to the store

Troubleshooting -- not needed normally:
    ai-kanban room sync                Sync now. Sessions and the web UI do it by themselves
    ai-kanban --version                This build and the sync protocol it speaks

Run by the plugin, not by you:
    ai-kanban mcp                      The MCP server an agent talks to, on stdio
    ai-kanban hook session-start | post-tool-use
    ai-kanban install-cli --link L     What /ai-kanban:install-cli runs (--dir <d>, --replace)

Everything you have recorded lives in one SQLite file outside version control. Keep it
with:

    ai-kanban backup ~/kanban-backup.db

Restore it by copying that file back over the store (`ai-kanban where` prints the path).

Do not just copy `kanban.db`: the store runs in WAL mode, so recent work can still be in
a `kanban.db-wal` sidecar, and a copy of the main file alone silently leaves it behind.
`backup` goes through SQLite and cannot lose it.

`export` is the other direction -- JSON you can read, diff and commit, for one project or
all of them. `import` restores projects that are not here yet; it will not merge into a
project that already exists.

Override the store location with AI_KANBAN_DB.
";

fn version() -> String {
    format!("ai-kanban {} (protocol {})", env!("CARGO_PKG_VERSION"), ai_kanban::sync::protocol::PROTOCOL_VERSION)
}

#[tokio::main]
async fn main() {
    // Core errors go through `render::error` here for the same reason the MCP and HTTP
    // adapters do it: those messages state what went wrong, what exists instead, and what
    // to do next. Letting `?` bubble a core error out of `main` prints the derived Debug
    // form -- `AmbiguousProject { query: "widget", candidates: [...] }` -- which buries the
    // list of candidates the person needs in Rust struct syntax.
    if let Err(e) = run().await {
        match e.downcast::<ai_kanban::core::Error>() {
            Ok(core) => eprint!("{}", ai_kanban::render::safe(&ai_kanban::render::error(&core))),
            Err(other) => eprintln!("ai-kanban: {other}"),
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let command = std::env::args().nth(1);
    if !matches!(command.as_deref(), Some("mcp" | "serve" | "hook")) {
        quiet_on_closed_pipe();
    }
    match command.as_deref() {
        Some("mcp") => run_mcp().await,
        // Sharing boards through a relay (docs/sync.md). A person's commands, never the agent's.
        Some("remote") => ai_kanban::remote::cli::remote_cmd(),
        Some("room") => ai_kanban::remote::cli::room_cmd(),
        #[cfg(unix)]
        Some("install-cli") => install_cli(),
        Some("serve") => {
            // Loopback only, and no flag to change that: the store is global and there is no
            // auth. See `http::serve`.
            let port = std::env::args()
                .skip_while(|a| a != "--port")
                .nth(1)
                .and_then(|p| p.parse().ok())
                .unwrap_or(7373);
            ai_kanban::http::serve(Store::open_default()?, port).await
        }
        // Hooks are invoked by the host, never by a person. They read hook JSON on stdin
        // and write hook JSON on stdout, so the plugin's wrapper only has to locate this
        // binary -- it never parses or rewrites the protocol.
        Some("hook") => {
            match std::env::args().nth(2).as_deref() {
                Some("session-start") => hook::session_start(),
                Some("post-tool-use") => hook::post_tool_use(),
                // Silence, not an error: a hook that complains on every session start is a
                // hook the user removes, taking the bundled MCP server with it.
                _ => {}
            }
            Ok(())
        }
        Some("backup") => {
            let Some(dest) = std::env::args().nth(2) else {
                eprintln!("usage: ai-kanban backup <file>");
                std::process::exit(2);
            };
            let store = Store::open_default()?;
            store.backup_to(std::path::Path::new(&dest))?;
            println!("Backed up to {dest}");
            Ok(())
        }
        Some("export") => {
            // Positional project names/keys, so `export ai-kanban` reads the way it looks.
            let keys: Vec<String> = std::env::args().skip(2).collect();
            let store = Store::open_default()?;
            let data = store.export(&keys)?;
            // Pretty, not compact: the reason to choose JSON over `backup` is that a person
            // can read and diff it, and one line of minified JSON is neither.
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
        Some("import") => {
            let Some(src) = std::env::args().nth(2) else {
                eprintln!("usage: ai-kanban import <file>");
                std::process::exit(2);
            };
            let data: ai_kanban::core::transfer::Export =
                serde_json::from_str(&std::fs::read_to_string(&src)?)?;
            let store = Store::open_default()?;
            let report = store.import(&data)?;
            print!("{}", ai_kanban::render::safe(&ai_kanban::render::import_report(&report)));
            Ok(())
        }
        // The spellings from before `board list` and `board merge`. Kept working: boards' notes
        // and logs -- the agents' memory -- name them, and must keep meaning something.
        Some("projects") => board_list(),
        Some("merge") => board_merge(std::env::args().nth(2), std::env::args().nth(3)),
        Some("repo") => repo_cmd(),
        Some("board") => board_cmd(),
        Some("where") => {
            println!("{}", Store::default_path()?.display());
            Ok(())
        }
        // Which build this is, with the protocol it speaks: what to say when asking for help,
        // and what decides whether it can talk to a given relay (`ai-kanban remote` shows that).
        Some("--version") | Some("-V") | Some("version") => {
            println!("{}", version());
            Ok(())
        }
        Some("--help") | Some("-h") | Some("help") | None => {
            println!("{}\n", version());
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

/// Arguments after the subcommand, with the flags these commands take pulled out: `--board
/// <name>` picks a board other than this directory's, `--yes` confirms a forget.
struct CliArgs {
    pos: Vec<String>,
    board: Option<String>,
    yes: bool,
    all: bool,
}

fn cli_args() -> CliArgs {
    let mut out = CliArgs { pos: vec![], board: None, yes: false, all: false };
    let mut it = std::env::args().skip(2);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--board" => out.board = it.next(),
            "--yes" => out.yes = true,
            "--all" => out.all = true,
            _ => out.pos.push(a),
        }
    }
    out
}

impl CliArgs {
    fn arg(&self, n: usize) -> Option<&str> {
        self.pos.get(n).map(String::as_str).map(str::trim).filter(|s| !s.is_empty())
    }
}

fn usage(line: &str) -> ! {
    eprintln!("usage: {line}");
    std::process::exit(2);
}

/// `board add | forget`. Named boards are for a project that is not one folder -- a customer,
/// a tracker's project -- and their repos are what agents reach them through.
///
/// Here rather than in the web UI or the agent's tools: the web UI is for looking at what the
/// agent recorded, not for running the work, and creating or destroying a board is a rare,
/// deliberate act that belongs to the person, like `merge`.
/// `board list`: no cap -- the point of this listing is spotting a board that should not
/// exist, and a truncated list is exactly where a split would hide.
fn board_list() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::open_default()?;
    let (sums, _) = store.project_summaries(usize::MAX)?;
    print!("{}", ai_kanban::render::safe(&ai_kanban::render::project_list(&sums, ai_kanban::core::now())));
    Ok(())
}

/// `board merge`: both boards named explicitly, and the first one survives. A merge is
/// irreversible and it deletes a board, so it must never be inferred from where the caller
/// happens to be standing.
fn board_merge(keep: Option<String>, gone: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let (Some(keep), Some(gone)) = (keep, gone) else {
        eprintln!("usage: ai-kanban board merge <board-to-keep> <board-to-merge-in>");
        eprintln!("\nBoth are a board name or key -- `ai-kanban board list` lists them.");
        std::process::exit(2);
    };
    let store = Store::open_default()?;
    let into = store.project_by_name_or_key(&keep)?;
    let from = store.project_by_name_or_key(&gone)?;
    let report = store.merge_projects(into.id, from.id)?;
    print!("{}", ai_kanban::render::safe(&ai_kanban::render::merge_report(&report)));
    Ok(())
}

fn board_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let a = cli_args();
    if matches!(a.arg(0), Some("list")) {
        return board_list();
    }
    if matches!(a.arg(0), Some("merge")) {
        return board_merge(a.arg(1).map(String::from), a.arg(2).map(String::from));
    }
    let store = Store::open_default()?;
    match a.arg(0) {
        Some("add") => {
            let Some(name) = a.arg(1) else { usage("ai-kanban board add <name>") };
            let p = store.create_board(name)?;
            println!("board {} created. Give it repos with: ai-kanban repo add <path> --board {}", p.name, p.name);
            Ok(())
        }
        Some("forget") => {
            let Some(name) = a.arg(1) else { usage("ai-kanban board forget <board> [--yes]") };
            let p = store.project_by_name_or_key(name)?;
            if !a.yes {
                // A dry run first, because this cannot be undone and a board name comes back
                // out of shell history far too easily.
                let (sums, _) = store.project_summaries(usize::MAX)?;
                let open = sums.iter().find(|s| s.project.id == p.id).map(|s| s.open).unwrap_or(0);
                let repos = store.repo_summaries(p.id)?;
                println!("would forget board {} ({}) with every task, note and event -- {open} open task{}.",
                    p.name, p.key, if open == 1 { "" } else { "s" });
                for r in &repos {
                    let home = r.repo.home_project_id == p.id;
                    println!("  repo {}: {}", r.repo.name, match (home, r.other_boards.is_empty()) {
                        (true, true) => "released; its folder starts a fresh board next time",
                        (true, false) => "moves home to another board that has it",
                        (false, _) => "stays on its home board",
                    });
                }
                println!("\nNot recoverable except from a backup. Rerun with --yes to do it.");
                std::process::exit(1);
            }
            store.forget_board(p.id)?;
            println!("forgot board {}", p.name);
            Ok(())
        }
        _ => usage("ai-kanban board add <name> | forget <board> [--yes]"),
    }
}

/// `repo list | add | rm | rename | home | forget`. The board is the one this directory
/// belongs to unless `--board` names another -- the same default the MCP tools use, because a
/// person running this usually stands in the checkout they mean.
///
/// `add` may bring a board into being, because registering a checkout is exactly the act
/// that says "this directory is work"; the others never do, so running them in some
/// unrelated folder cannot leave an empty board behind.
fn repo_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let a = cli_args();
    let cwd = std::env::current_dir()?;
    let store = Store::open_default()?;
    let user = ai_kanban::core::model::Actor::User;

    // The board a command acts on: `--board`, else this directory's, never created.
    let board = |store: &Store| -> Result<ai_kanban::core::model::Project, Box<dyn std::error::Error>> {
        if let Some(b) = &a.board {
            return Ok(store.project_by_name_or_key(b)?);
        }
        match store.find_project(&cwd)? {
            Some(p) => Ok(p),
            None => Err(Box::new(ai_kanban::core::Error::NoProjectContext)),
        }
    };
    // A repo on that board, by name or path, through the same resolver the MCP tools use.
    let on_board = |store: &Store, project_id: i64, name: &str| -> Result<i64, Box<dyn std::error::Error>> {
        store.resolve_repos(project_id, &[name.to_string()])?.first().copied().ok_or_else(|| {
            Box::new(ai_kanban::core::Error::InvalidValue {
                field: "repo",
                value: name.to_string(),
                valid: "a repo on this board -- `ai-kanban repo list` shows them".into(),
            }) as Box<dyn std::error::Error>
        })
    };

    match a.arg(0) {
        Some("list") | None => {
            if a.arg(1).is_some_and(|x| x.eq_ignore_ascii_case("all")) {
                let homes: std::collections::HashMap<i64, String> =
                    store.all_projects()?.into_iter().map(|p| (p.id, p.name)).collect();
                let repos: Vec<_> = store.all_repos()?.into_iter()
                    .map(|r| { let h = homes.get(&r.home_project_id).cloned().unwrap_or_default(); (r, h) })
                    .collect();
                print!("{}", ai_kanban::render::safe(&ai_kanban::render::repo_directory(&repos)));
                return Ok(());
            }
            let project = match a.arg(1) {
                Some(name) => store.project_by_name_or_key(name)?,
                None => board(&store)?,
            };
            print!("{}", ai_kanban::render::safe(&ai_kanban::render::repo_menu(&project.name, &store.repo_summaries(project.id)?)));
            Ok(())
        }
        Some("add") => {
            let Some(path) = a.arg(1) else { usage("ai-kanban repo add <path> [name] [--board <board>]") };
            let project = match &a.board {
                Some(b) => store.project_by_name_or_key(b)?,
                None => store.resolve_project(&cwd)?.project,
            };
            let repo = store.add_repo(project.id, &ai_kanban::expand_home(path), a.arg(2), user)?;
            println!("{} -> {} (board {})", repo.name, repo.path, project.name);
            Ok(())
        }
        // Off every board is `forget` under its older name, kept working for what notes say.
        Some("rm") if a.all => forget_repo(&store, a.arg(1), a.yes),
        Some("forget") => forget_repo(&store, a.arg(1), a.yes),
        Some("rm") => {
            let Some(name) = a.arg(1) else { usage("ai-kanban repo rm <repo> [--board <board> | --all]") };
            let project = board(&store)?;
            let id = on_board(&store, project.id, name)?;
            let repo = store.repo(project.id, id)?;
            let unlinked = store.remove_repo(project.id, id, user)?;
            println!("{} removed from {}, off {unlinked} ticket{}",
                repo.name, project.name, if unlinked == 1 { "" } else { "s" });
            Ok(())
        }
        Some("rename") => {
            let (Some(name), Some(new)) = (a.arg(1), a.arg(2)) else {
                usage("ai-kanban repo rename <repo> <new-name> [--board <board>]")
            };
            let project = board(&store)?;
            let id = on_board(&store, project.id, name)?;
            let repo = store.rename_repo(project.id, id, new, user)?;
            println!("renamed to {}", repo.name);
            Ok(())
        }
        // Not in the help: which board a folder shared by several boards opens on is the first
        // one it was added to, and that is nearly always right. Kept for the rare case.
        Some("home") => {
            // Both named: moving a home changes where an agent opening that folder lands, so it
            // is never inferred from the directory the command happens to run in.
            let (Some(name), Some(to)) = (a.arg(1), a.arg(2)) else {
                usage("ai-kanban repo home <repo> <board>")
            };
            let repo = store.find_repo(name)?;
            let project = store.project_by_name_or_key(to)?;
            let repo = store.set_repo_home(project.id, repo.id, user)?;
            println!("{} now opens on board {}", repo.name, project.name);
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown repo command: {other}\n\nusage: ai-kanban repo list|add|rm|rename");
            std::process::exit(2);
        }
    }
}

async fn run_mcp() -> Result<(), Box<dyn std::error::Error>> {
    // stdout is the MCP transport, so every diagnostic must go to stderr. Printing to
    // stdout here would inject garbage into the protocol stream and break the session in a
    // way that is genuinely hard to diagnose from the client side.
    let store = Store::open_default()?;
    eprintln!("ai-kanban: store at {}", Store::default_path()?.display());

    // A relay in the plugin's settings becomes this machine's remote if it has none (local
    // only: the first sync round asks the relay who the key is). One that differs is not taken,
    // and the agent is told so. A failure here must not cost the session its board.
    use ai_kanban::remote::config::{adopt_plugin_settings, Adoption};
    let notice = match adopt_plugin_settings() {
        Ok(Adoption::Adopted(cfg)) => {
            eprintln!("ai-kanban: relay {} from the plugin settings saved as this machine's remote", cfg.url);
            None
        }
        Ok(Adoption::Kept { machine }) => Some(ai_kanban::render::relay_settings_kept(&machine.url)),
        Ok(_) => None,
        Err(e) => {
            eprintln!("ai-kanban: could not save the relay from the plugin settings: {e}");
            None
        }
    };

    // Keeps the link `install-cli` points the terminal at on this binary, so the command in a
    // user's PATH follows plugin updates. Only under the plugin, and never at the session's cost.
    #[cfg(unix)]
    if let (Some(data), Ok(exe)) = (std::env::var_os("CLAUDE_PLUGIN_DATA"), std::env::current_exe()) {
        if let Err(e) = ai_kanban::install_cli::refresh_stable_link(std::path::Path::new(&data), &exe) {
            eprintln!("ai-kanban: could not update the terminal link in {}: {e}", std::path::Path::new(&data).display());
        }
    }

    // Shared boards sync in a background thread of this process (`remote::daemon`); nothing
    // starts unless a remote is set and this run allows sync.
    let sync = ai_kanban::remote::daemon::Syncer::start();
    let service = AiKanban::with_sync(store, sync).with_notice(notice).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// `install-cli`: what `/ai-kanban:install-cli` runs. Touches no store.
#[cfg(unix)]
fn install_cli() -> Result<(), Box<dyn std::error::Error>> {
    use ai_kanban::install_cli::{install, on_path, Installed, Refused};
    let args: Vec<String> = std::env::args().skip(2).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let link = value("--link").ok_or("usage: ai-kanban install-cli --link <plugin data>/bin/ai-kanban [--dir <dir>] [--replace]")?;
    let dir = value("--dir").map(|d| ai_kanban::expand_home(&d)).unwrap_or_else(|| ai_kanban::expand_home("~/.local/bin"));
    let at = dir.join("ai-kanban");
    match install(&dir, std::path::Path::new(&link), args.iter().any(|a| a == "--replace")) {
        Ok(Installed::Created) => println!("linked {} -> {link}", at.display()),
        Ok(Installed::Already) => println!("already linked: {} -> {link}", at.display()),
        Ok(Installed::Replaced) => println!("replaced what was at {} with a link to {link}", at.display()),
        Err(Refused::NoStableLink(p)) => {
            return Err(format!("{} does not exist yet: the plugin creates it when a Claude Code session starts. Start one, then run this again", p.display()).into());
        }
        Err(Refused::Occupied { at, what }) => {
            return Err(format!("{} is already there ({what}). Run again with --replace to put the link in its place", at.display()).into());
        }
    }
    println!("It follows plugin updates by itself: the plugin moves the link it points to.");
    if !on_path(&dir, &std::env::var("PATH").unwrap_or_default()) {
        println!(
            "\n{} is not on your PATH, so `ai-kanban` will not be found by name yet. Add to your shell profile:\n\n  export PATH=\"{}:$PATH\"",
            dir.display(),
            dir.display()
        );
    }
    Ok(())
}

/// Lets a closed pipe end a terminal command quietly, the way `ls | head` ends `ls`.
///
/// Rust ignores SIGPIPE, so writing to a reader that went away (`ai-kanban projects | head -1`)
/// makes `println!` panic with "failed printing to stdout: Broken pipe" instead. Restoring the
/// default disposition is the usual fix for command-line tools. Only for the commands a person
/// runs: `serve` and `mcp` hold sockets and pipes whose peers may hang up, and the hooks run
/// under Claude Code -- those keep Rust's default, where a hung-up peer is an error to handle,
/// never a reason for the process to die.
fn quiet_on_closed_pipe() {
    #[cfg(unix)]
    {
        // Declared here rather than pulled in from the `libc` crate: one call, standard on every
        // Unix this builds for, and SIGPIPE is 13 on Linux and macOS alike.
        unsafe extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        const SIGPIPE: i32 = 13;
        const SIG_DFL: usize = 0;
        // SAFETY: called once at startup, before any thread exists; SIG_DFL is always valid.
        unsafe {
            signal(SIGPIPE, SIG_DFL);
        }
    }
}

/// `repo rm --all`: a repo off every board and every ticket naming it. Its folder keeps
/// opening where it does now.
fn forget_repo(store: &Store, name: Option<&str>, yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    let Some(name) = name else { usage("ai-kanban repo rm <repo> --all [--yes]") };
    let repo = store.find_repo(name)?;
    if !yes {
        let summary = store.repo_summaries(repo.home_project_id)?.into_iter().find(|r| r.repo.id == repo.id);
        let boards = summary.as_ref().map(|s| 1 + s.other_boards.len()).unwrap_or(1);
        println!("would remove repo {} ({}): off {boards} board{} and every ticket naming it.",
            repo.name, repo.path, if boards == 1 { "" } else { "s" });
        println!("Its folder keeps opening where it does now.\n\nNot recoverable except from a backup. Rerun with --yes to do it.");
        std::process::exit(1);
    }
    store.forget_repo(repo.id)?;
    println!("removed repo {} from every board", repo.name);
    Ok(())
}
