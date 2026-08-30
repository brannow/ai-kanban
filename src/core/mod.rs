//! Core: SQLite + domain logic. Returns structured data, never prose.
//!
//!   core ──> structured data
//!     ├── MCP adapter  -> prose for agents
//!     └── HTTP API     -> JSON + the web UI, for a human

pub mod board;
pub mod error;
pub mod forget;
pub mod migrate;
pub mod model;
pub mod event;
pub mod note;
pub mod page;
pub mod project;
pub mod recall;
pub mod task;
pub mod store;

pub use error::{Error, Result};
pub use store::{now, Store};
