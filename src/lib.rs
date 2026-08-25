//! ai-kanban -- a kanban board whose first-class consumer is an AI agent.
//!
//! Layering (see docs/architecture.md):
//!
//!   core ──> structured data
//!     ├── render       -> prose, shared by every agent-facing adapter
//!     ├── MCP adapter  -> the tools an agent calls
//!     ├── hook adapter -> context injected without the agent asking
//!     └── HTTP API     -> JSON for a web UI (designed for, not built)
//!
//! Core never returns prose. That is not stylistic: the human's access to this data is
//! meant to be an API, and text in core would force every future consumer to parse
//! sentences back into objects.

pub mod core;
pub mod hook;
pub mod mcp;
pub mod render;
