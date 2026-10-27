//! The wire format between a client and the relay. `docs/sync.md` is the design; this is the
//! one place both sides take it from, so they cannot drift apart.
//!
//! # Versioning
//!
//! `PROTOCOL_VERSION` is deliberately **not** the schema version, for the reason
//! `transfer::FORMAT_VERSION` is not: the store gains columns for reasons that never touch the
//! wire, and tying the two together would force a protocol bump for every internal migration.
//! A mismatch pauses sync and never blocks local work, so the version only has to say what the
//! envelope looks like.
//!
//! # What the relay reads
//!
//! The envelope, never `value`. The server checks who wrote a change and which entity it belongs
//! to, and stores the rest unread. So a new status or field needs only clients that understand
//! it, and members can upgrade at their own pace.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The envelope version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest envelope version this build still accepts. Equal to the current one until the
/// envelope first changes; a breaking change gets a new route prefix instead (`/v2`).
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// The tag the server signs its own changes with -- admin forget tombstones. Reserved so no
/// key can be issued under it, which is what makes a `srv` change trustworthy.
pub const SERVER_TAG: &str = "srv";

/// Changes per push. A cap rather than a stream: the outbox drains in batches anyway, and an
/// unbounded batch is one request holding the write lock for as long as it likes.
pub const MAX_BATCH: usize = 1000;

/// Changes per pull page.
pub const MAX_PAGE: usize = 1000;

/// The entity kinds a change can be about. Task and note ids are separate sequences on every
/// store, so the kind is part of the identity, never a decoration.
pub const KINDS: &[&str] = &["task", "note", "event"];

/// Whether `tag` can be an origin tag: short, lowercase, starting with a letter, and without
/// `-`, because handles (`122-v1`) are parsed from strings by splitting on it.
pub fn valid_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some('a'..='z'))
        && tag.len() <= 8
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// A room name in its one stored spelling, or `None` if it cannot be one.
///
/// Lowercased, because people type it: `Sbahn` in one command and `sbahn` in the next must be
/// one room, not two that each look empty to half the team.
pub fn room_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_ascii_lowercase();
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some('a'..='z' | '0'..='9'))
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    ok.then_some(name)
}

/// An entity as every member knows it: who created it, and the id it got there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ref {
    pub kind: String,
    pub origin: String,
    pub id: i64,
}

impl Ref {
    pub fn valid(&self) -> bool {
        KINDS.contains(&self.kind.as_str()) && valid_tag(&self.origin) && self.id > 0
    }
}

/// A hybrid logical clock. Compared field by field in declaration order, so the derive is the
/// ordering: wall time, then the counter that breaks ties within a millisecond, then the tag
/// that breaks ties between machines. Not `updated_at`, which is whole seconds and ties all
/// the time (note #3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Clock {
    pub wall_ms: i64,
    pub counter: u32,
    pub tag: String,
}

/// One changed field of one entity, or one whole insert-only entity (an event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// `<tag>:<outbox seq>`. Stored once per server, so a push retried after a timeout cannot
    /// apply twice.
    pub cid: String,
    pub entity: Ref,
    /// What a forget of this change's subject removes. Mirrors `core::forget`: a note's own
    /// events have the note as subject even when they also sit on a task, because forgetting
    /// a task detaches its notes instead of deleting them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Ref>,
    /// Absent for an insert-only entity, whose `value` is the whole row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Never read by the server.
    #[serde(default)]
    pub value: serde_json::Value,
    pub clock: Clock,
    /// Envelope fields this build does not know, kept and passed on. Without this a relay older
    /// than its clients would strip what a newer client added, and an additive envelope change
    /// would need a new route like a breaking one. Maps serialize with sorted keys, so the
    /// relay's duplicate check stays deterministic.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Change {
    /// The tag and outbox seq a `cid` is made of.
    pub fn cid_parts(&self) -> Option<(&str, i64)> {
        let (tag, seq) = self.cid.split_once(':')?;
        let seq: i64 = seq.parse().ok()?;
        (seq > 0).then_some((tag, seq))
    }
}

/// A task as a teammate names it: `122-v1`, or `#122-v1` as it is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub id: i64,
    pub origin: String,
}

impl Handle {
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().trim_start_matches('#');
        let (id, origin) = s.split_once('-')?;
        let id: i64 = id.parse().ok()?;
        (id > 0 && valid_tag(origin)).then(|| Handle { id, origin: origin.to_string() })
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}-{}", self.id, self.origin)
    }
}

// ---------------------------------------------------------------------------
// Request and response bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub server_version: String,
}

/// The highest counters the server has accepted from one tag. A client whose own counters are
/// lower was restored from a backup or copied, and would reissue ids the room already has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighWater {
    pub outbox: i64,
    pub task: i64,
    pub note: i64,
    pub event: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmI {
    pub user: String,
    pub tag: String,
    pub high_water: HighWater,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub tag: String,
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomInfo {
    pub name: String,
    pub created_at: i64,
    pub created_by: String,
    pub members: Vec<Member>,
    pub changes: i64,
    pub head: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Joined {
    pub room: RoomInfo,
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Push {
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pushed {
    pub accepted: usize,
    /// Already stored under the same `cid` with the same content: a retry.
    pub duplicates: usize,
    /// About an entity that was forgotten in this room. Dropped so it cannot come back.
    pub forgotten: usize,
    pub head: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pulled {
    pub seq: i64,
    pub author: String,
    pub received_at: i64,
    pub change: Change,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub changes: Vec<Pulled>,
    pub head: i64,
    /// More changes exist past this page; pull again from the last `seq`.
    pub more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_cannot_contain_what_handles_split_on() {
        assert!(valid_tag("v1"));
        assert!(valid_tag("benny"));
        assert!(!valid_tag("b-1"));
        assert!(!valid_tag("1b"));
        assert!(!valid_tag("B1"));
        assert!(!valid_tag(""));
        assert!(!valid_tag("toolongtag"));
    }

    #[test]
    fn handles_parse_with_or_without_the_hash() {
        let h = Handle::parse("#122-v1").unwrap();
        assert_eq!((h.id, h.origin.as_str()), (122, "v1"));
        assert_eq!(Handle::parse("122-v1"), Some(h.clone()));
        assert_eq!(h.to_string(), "#122-v1");
        assert_eq!(Handle::parse("122"), None);
        assert_eq!(Handle::parse("0-v1"), None);
        assert_eq!(Handle::parse("12-V1"), None);
    }

    #[test]
    fn clocks_order_by_wall_then_counter_then_tag() {
        let c = |w, n, t: &str| Clock { wall_ms: w, counter: n, tag: t.into() };
        assert!(c(1, 9, "z") < c(2, 0, "a"));
        assert!(c(1, 1, "z") < c(1, 2, "a"));
        assert!(c(1, 1, "a") < c(1, 1, "b"));
    }

    #[test]
    fn room_names_have_one_spelling() {
        assert_eq!(room_name(" Sbahn "), Some("sbahn".into()));
        assert_eq!(room_name("sbahn-v13"), Some("sbahn-v13".into()));
        assert_eq!(room_name("-x"), None);
        assert_eq!(room_name("a b"), None);
    }

    #[test]
    fn cid_parts_need_a_positive_seq() {
        let mut ch = Change {
            cid: "b1:12".into(),
            entity: Ref { kind: "task".into(), origin: "b1".into(), id: 1 },
            subject: None,
            field: None,
            value: serde_json::Value::Null,
            clock: Clock { wall_ms: 0, counter: 0, tag: "b1".into() },
            extra: Default::default(),
        };
        assert_eq!(ch.cid_parts(), Some(("b1", 12)));
        ch.cid = "b1:0".into();
        assert_eq!(ch.cid_parts(), None);
        ch.cid = "b1".into();
        assert_eq!(ch.cid_parts(), None);
    }
}
