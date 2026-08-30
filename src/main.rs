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
