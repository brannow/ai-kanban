//! ai-kanban -- a kanban board whose first-class consumer is an AI agent.
//!
//! Layering (see docs/architecture.md):
//!
//!   core ──> structured data
//!     ├── render       -> prose, shared by every agent-facing adapter
//!     ├── MCP adapter  -> the tools an agent calls
//!     ├── hook adapter -> context injected without the agent asking
//!     └── HTTP API     -> JSON + the web UI, for a human
//!
//! Core never returns prose. That is not stylistic: the human's access to this data is
//! meant to be an API, and text in core would force every future consumer to parse
//! sentences back into objects.

pub mod core;
pub mod hook;
pub mod http;
pub mod mcp;
pub mod render;

/// `~/...` expanded against the home directory.
///
/// Above core and shared by every adapter rather than living in one of them: a path typed
/// into the web form, handed to an MCP tool by an agent, or quoted on a command line can all
/// carry a `~` nothing has expanded yet, while core takes real paths. One implementation, so
/// the three surfaces cannot disagree about what `~` means.
pub fn expand_home(raw: &str) -> std::path::PathBuf {
    let rest = if raw == "~" { Some("") } else { raw.strip_prefix("~/") };
    match (rest, dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => raw.into(),
    }
}
