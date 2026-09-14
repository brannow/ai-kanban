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
    ai-kanban repo list [board]    Repos on a board, or \"all\" for every repo in the store
    ai-kanban repo add <path>      Register a checkout on this directory's board
    ai-kanban repo rm <repo>       Take a repo off this directory's board
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

/// `repo list | add | rm`. The board is the one this directory belongs to, the same rule the
/// MCP tools follow -- a person running this stands in the checkout they mean.
///
/// `add` may bring a board into being, because registering a checkout is exactly the act
/// that says "this directory is work"; `list` and `rm` never do, so running them in some
/// unrelated folder cannot leave an empty board behind.
fn repo_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let arg = |n: usize| args.get(n).map(String::as_str).map(str::trim).filter(|s| !s.is_empty());
    let cwd = std::env::current_dir()?;
    let store = Store::open_default()?;

    let here = |store: &Store| -> Result<ai_kanban::core::model::Project, Box<dyn std::error::Error>> {
        match store.find_project(&cwd)? {
            Some(p) => Ok(p),
            None => Err(Box::new(ai_kanban::core::Error::NoProjectContext)),
        }
    };

    match arg(0) {
        Some("list") | None => {
            if arg(1).is_some_and(|a| a.eq_ignore_ascii_case("all")) {
                let homes: std::collections::HashMap<i64, String> =
                    store.all_projects()?.into_iter().map(|p| (p.id, p.name)).collect();
                let repos: Vec<_> = store.all_repos()?.into_iter()
                    .map(|r| { let h = homes.get(&r.home_project_id).cloned().unwrap_or_default(); (r, h) })
                    .collect();
                print!("{}", ai_kanban::render::repo_directory(&repos));
                return Ok(());
            }
            let project = match arg(1) {
                Some(name) => store.project_by_name_or_key(name)?,
                None => here(&store)?,
            };
            print!("{}", ai_kanban::render::repo_menu(&project.name, &store.repo_summaries(project.id)?));
            Ok(())
        }
        Some("add") => {
            let Some(path) = arg(1) else {
                eprintln!("usage: ai-kanban repo add <path> [name]");
                std::process::exit(2);
            };
            let project = store.resolve_project(&cwd)?.project;
            let repo = store.add_repo(
                project.id, &ai_kanban::expand_home(path), arg(2), ai_kanban::core::model::Actor::User,
            )?;
            println!("{} -> {} (board {})", repo.name, repo.path, project.name);
            Ok(())
        }
        Some("rm") => {
            let Some(name) = arg(1) else {
                eprintln!("usage: ai-kanban repo rm <repo>");
                std::process::exit(2);
            };
            let project = here(&store)?;
            // Resolved by name or path through core, so the CLI and the MCP tool accept the
            // same spellings and give the same error when one is wrong.
            let ids = store.resolve_repos(project.id, &[name.to_string()])?;
            let Some(&id) = ids.first() else {
                return Err(Box::new(ai_kanban::core::Error::InvalidValue {
                    field: "repo",
                    value: name.to_string(),
                    valid: "a repo on this board -- `ai-kanban repo list` shows them".into(),
                }));
            };
            let repo = store.repo(project.id, id)?;
            let unlinked = store.remove_repo(project.id, id, ai_kanban::core::model::Actor::User)?;
            println!("{} removed from {}, off {unlinked} ticket{}",
                repo.name, project.name, if unlinked == 1 { "" } else { "s" });
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown repo command: {other}\n\nusage: ai-kanban repo list|add|rm");
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
