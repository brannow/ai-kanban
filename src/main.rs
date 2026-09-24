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

USAGE:
    ai-kanban serve [--port N]     Web UI + HTTP API on http://127.0.0.1:7373
    ai-kanban mcp                  Run the MCP server on stdio (what an agent connects to)
    ai-kanban hook session-start   Emit the board as Claude Code SessionStart context
    ai-kanban hook post-tool-use   Emit notes about the file a tool just touched
    ai-kanban backup <file>        Copy the whole store to one consistent file
    ai-kanban export [project...]  Write the store as JSON on stdout
    ai-kanban import <file>        Restore projects from an export
    ai-kanban projects             List every board in the store
    ai-kanban board add <name>     Create a board for a project that is not one folder
    ai-kanban board forget <board> Delete a board and all its history (dry run without --yes)
    ai-kanban repo list [board]    Repos on a board, or \"all\" for every repo in the store
    ai-kanban repo add <path>      Register a checkout on this directory's board (--board <b>)
    ai-kanban repo rm <repo>       Take a repo off this directory's board (--board <b>)
    ai-kanban repo rename <repo> <new>   Rename a repo (--board <b>)
    ai-kanban repo home <repo> <board>   Make its folder open on that board from now on
    ai-kanban repo forget <repo>   Remove a repo from every board (dry run without --yes)
    ai-kanban merge <keep> <gone>  Repair a board that split into two
    ai-kanban where                Print the path to the store
    ai-kanban --help

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

#[tokio::main]
async fn main() {
    // Core errors go through `render::error` here for the same reason the MCP and HTTP
    // adapters do it: those messages state what went wrong, what exists instead, and what
    // to do next. Letting `?` bubble a core error out of `main` prints the derived Debug
    // form -- `AmbiguousProject { query: "widget", candidates: [...] }` -- which buries the
    // list of candidates the person needs in Rust struct syntax.
    if let Err(e) = run().await {
        match e.downcast::<ai_kanban::core::Error>() {
            Ok(core) => eprint!("{}", ai_kanban::render::error(&core)),
            Err(other) => eprintln!("ai-kanban: {other}"),
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("mcp") => run_mcp().await,
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
            print!("{}", ai_kanban::render::import_report(&report));
            Ok(())
        }
        Some("projects") => {
            let store = Store::open_default()?;
            // No cap: the point of this listing is spotting a board that should not exist,
            // and a truncated list is exactly where a split would hide.
            let (sums, _) = store.project_summaries(usize::MAX)?;
            print!("{}", ai_kanban::render::project_list(&sums, ai_kanban::core::now()));
            Ok(())
        }
        Some("merge") => {
            // Both boards are named explicitly, and the first one survives. A merge is
            // irreversible and it deletes a board, so it must never be inferred from where
            // the caller happens to be standing.
            let (keep, gone) = (std::env::args().nth(2), std::env::args().nth(3));
            let (Some(keep), Some(gone)) = (keep, gone) else {
                eprintln!("usage: ai-kanban merge <board-to-keep> <board-to-merge-in>");
                eprintln!("\nBoth are a board name or key -- `ai-kanban projects` lists them.");
                std::process::exit(2);
            };
            let store = Store::open_default()?;
            let into = store.project_by_name_or_key(&keep)?;
            let from = store.project_by_name_or_key(&gone)?;
            let report = store.merge_projects(into.id, from.id)?;
            print!("{}", ai_kanban::render::merge_report(&report));
            Ok(())
        }
        Some("repo") => repo_cmd(),
        Some("board") => board_cmd(),
        Some("where") => {
            println!("{}", Store::default_path()?.display());
            Ok(())
        }
        Some("--help") | Some("-h") | Some("help") | None => {
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
}

fn cli_args() -> CliArgs {
    let mut out = CliArgs { pos: vec![], board: None, yes: false };
    let mut it = std::env::args().skip(2);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--board" => out.board = it.next(),
            "--yes" => out.yes = true,
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
fn board_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let a = cli_args();
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
                print!("{}", ai_kanban::render::repo_directory(&repos));
                return Ok(());
            }
            let project = match a.arg(1) {
                Some(name) => store.project_by_name_or_key(name)?,
                None => board(&store)?,
            };
            print!("{}", ai_kanban::render::repo_menu(&project.name, &store.repo_summaries(project.id)?));
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
        Some("rm") => {
            let Some(name) = a.arg(1) else { usage("ai-kanban repo rm <repo> [--board <board>]") };
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
        Some("forget") => {
            let Some(name) = a.arg(1) else { usage("ai-kanban repo forget <repo> [--yes]") };
            let repo = store.find_repo(name)?;
            if !a.yes {
                let summary = store.repo_summaries(repo.home_project_id)?.into_iter().find(|r| r.repo.id == repo.id);
                let boards = summary.as_ref().map(|s| 1 + s.other_boards.len()).unwrap_or(1);
                println!("would forget repo {} ({}): off {boards} board{} and every ticket naming it.",
                    repo.name, repo.path, if boards == 1 { "" } else { "s" });
                println!("Its folder keeps opening where it does now.\n\nNot recoverable except from a backup. Rerun with --yes to do it.");
                std::process::exit(1);
            }
            store.forget_repo(repo.id)?;
            println!("forgot repo {}", repo.name);
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown repo command: {other}\n\nusage: ai-kanban repo list|add|rm|rename|home|forget");
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

    let service = AiKanban::new(store).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
