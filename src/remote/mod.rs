//! The client's side of the network: which relay this machine talks to, the HTTP calls, and
//! one round of sync for a board. `docs/sync.md` is the design.
//!
//! Kept out of `core`, which is SQLite plus domain logic and never touches the network. Core
//! turns local writes into changes and applies received ones (`core::sync`); this module only
//! carries them, and never holds a write transaction open while waiting on the network: every
//! agent session on the machine shares the store, and would queue behind it.

pub mod cli;
pub mod client;
pub mod config;
pub mod daemon;
pub mod engine;

pub use client::{Client, RemoteError};
pub use config::RemoteConfig;
pub use engine::{check_identity, sync_board, Synced};
