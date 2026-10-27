//! Sharing a board through a room on a relay. `docs/sync.md` is the design.
//!
//! `protocol` is the part client and server share. The server lives in `crate::server`; the
//! client side does not exist yet.

pub mod protocol;
