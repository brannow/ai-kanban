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
    ai-kanban mcp                  Run the MCP server on stdio (what an agent connects to)
    ai-kanban hook session-start   Emit the board as Claude Code SessionStart context
    ai-kanban hook post-tool-use   Emit notes about the file a tool just touched
    ai-kanban where                Print the path to the store
    ai-kanban --help

The store is a single SQLite file, but it runs in WAL mode, so recent work lives in a
`-wal` sidecar until it is checkpointed. Copying `kanban.db` on its own can therefore
silently leave the newest tasks and notes behind. Back it up with:

    sqlite3 \"$(ai-kanban where)\" \".backup /path/to/backup.db\"

Override the store location with AI_KANBAN_DB.
";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("mcp") => run_mcp().await,
        // Hooks are invoked by the host, never by a person. They read hook JSON on stdin
        // and write hook JSON on stdout, so the plugin needs no wrapper script.
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
