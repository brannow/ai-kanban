//! Core: SQLite + domain logic. Returns structured data, never prose.
//!
//!   core ──> structured data
//!     ├── MCP adapter  -> prose for agents
//!     └── HTTP API     -> JSON + the web UI, for a human

pub mod board;
pub mod error;
pub mod forget;
pub mod merge;
pub mod migrate;
pub mod model;
pub mod event;
pub mod note;
pub mod page;
pub mod profile;
pub mod project;
pub mod recall;
pub mod repo;
pub mod task;
pub mod todo;
pub mod staleness;
pub mod store;
pub mod transfer;
pub mod workstream;

pub use error::{Error, Result};
pub use store::{now, Store};
