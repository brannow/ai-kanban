//! ai-kanban -- a kanban board whose first-class consumer is an AI agent.
//!
//! Layering (see docs/architecture.md):
//!
//!   core ──> structured data
//!     ├── MCP adapter  -> prose for agents
//!     └── HTTP API     -> JSON for a web UI (designed for, not built)
//!
//! Core never returns prose. That is not stylistic: the human's access to this data is
//! meant to be an API, and text in core would force every future consumer to parse
//! sentences back into objects.

pub mod core;
pub mod mcp;
