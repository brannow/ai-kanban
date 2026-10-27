//! Sharing a board through a room on a relay. `docs/sync.md` is the design.
//!
//! The wire format lives in its own crate, `ai-kanban-protocol`, which the relay
//! (`crates/relay`) uses too: one definition, so client and relay cannot drift apart. Re-exported
//! here so the client names it as part of itself.

pub use ai_kanban_protocol as protocol;
