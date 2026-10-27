//! One round of sync for a board: publish what is pending, then apply what the room has.
//!
//! Safe to run from several processes at once on one store -- every agent session runs its own.
//! Nothing here holds the database across a network call; what makes concurrent rounds harmless
//! is already in the data: the relay stores each change id once, an older clock never wins, and
//! the cursor only moves forward.

use super::client::{Client, RemoteError};
use crate::core::Store;
use crate::sync::protocol::{self, MAX_BATCH, MAX_PAGE};

/// What one round did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    pub pushed: usize,
    pub applied: usize,
    pub parked: usize,
}

/// Whether this store may sync with this relay at all. A failure here pauses sync for the
/// store (`Store::paused`) and says what to do; local work is never affected.
///
/// Three reasons, each a state that would corrupt the room if sync went on: the relay speaks a
/// protocol this build does not; the key belongs to a different origin tag than this store
/// publishes under; or the relay has accepted higher ids from this tag than this store would
/// hand out next -- a store restored from a backup, whose next tasks would collide with ones
/// the room already holds.
pub fn check_identity(store: &Store, client: &Client) -> Result<(), RemoteError> {
    let hello = client.hello()?;
    if !(hello.protocol_min..=hello.protocol_max).contains(&protocol::PROTOCOL_VERSION) {
        let why = format!(
            "the relay speaks protocol {}-{}, this ai-kanban speaks {}. Upgrade ai-kanban (or the relay)",
            hello.protocol_min, hello.protocol_max, protocol::PROTOCOL_VERSION
        );
        pause(store, &why);
        return Err(RemoteError::Paused(why));
    }
    let who = client.whoami()?;
    let tag = store.origin_tag().ok().flatten();
    if tag.as_deref() != Some(who.tag.as_str()) {
        let why = format!(
            "the key is for origin tag {}, but this store publishes as {}. Run `ai-kanban remote <url> --key <key>` with this machine's key",
            who.tag,
            tag.as_deref().unwrap_or("nothing yet")
        );
        pause(store, &why);
        return Err(RemoteError::Paused(why));
    }
    let local = store.local_high_water().map_err(|e| RemoteError::Http(0, e.to_string()))?;
    let hw = &who.high_water;
    if local.outbox < hw.outbox || local.task < hw.task || local.note < hw.note || local.event < hw.event {
        let why = format!(
            "this store is behind what the relay already has from {tag} (restored from a backup, or copied?), so \
             its next tasks would reuse numbers the room holds. Disconnect its boards (`ai-kanban room disconnect`), \
             ask the relay admin for a key with a new tag, run `ai-kanban remote <url> --key <new key>`, then connect again",
            tag = who.tag
        );
        pause(store, &why);
        return Err(RemoteError::Paused(why));
    }
    let _ = store.set_paused(None);
    Ok(())
}

fn pause(store: &Store, why: &str) {
    let _ = store.set_paused(Some(why));
}

/// One round for `project_id`: flush, push the outbox, pull and apply until caught up, and
/// record who is in the room. Returns what moved.
pub fn sync_board(store: &Store, client: &Client, project_id: i64) -> Result<Synced, RemoteError> {
    let result = round(store, client, project_id);
    match &result {
        // A pause is the store's state, reported once by the header, not as this board's error.
        Err(RemoteError::Paused(_)) | Ok(_) => {}
        // A reused change id means this store was restored: stop until it has a new tag.
        Err(RemoteError::Conflict(msg)) => {
            pause(store, msg);
            let _ = store.sync_failed(project_id, msg);
        }
        Err(e) => {
            let _ = store.sync_failed(project_id, &e.to_string());
        }
    }
    result
}

fn round(store: &Store, client: &Client, project_id: i64) -> Result<Synced, RemoteError> {
    let core = |e: crate::core::Error| RemoteError::Http(0, e.to_string());
    if let Some(why) = store.paused().map_err(core)? {
        return Err(RemoteError::Paused(why));
    }
    let room = store.room_of(project_id).map_err(core)?.ok_or_else(|| RemoteError::NotFound("this board is not connected to a room".into()))?;
    let mut done = Synced::default();

    store.flush().map_err(core)?;
    loop {
        let out = store.outbox(project_id, MAX_BATCH).map_err(core)?;
        let Some(&(last, _)) = out.last() else { break };
        let changes: Vec<_> = out.into_iter().map(|(_, c)| c).collect();
        client.push(&room, &changes)?;
        store.outbox_sent(project_id, last).map_err(core)?;
        done.pushed += changes.len();
    }

    loop {
        let since = store.sync_status(project_id).map_err(core)?.map(|s| s.cursor).unwrap_or(0);
        let page = client.pull(&room, since, MAX_PAGE)?;
        if page.changes.is_empty() {
            break;
        }
        let r = store.apply(project_id, &page.changes).map_err(core)?;
        done.applied += r.applied;
        done.parked += r.parked;
        if !page.more {
            break;
        }
    }
    // Applying can have flushed pending edits made meanwhile; send them now rather than a round
    // later.
    loop {
        let out = store.outbox(project_id, MAX_BATCH).map_err(core)?;
        let Some(&(last, _)) = out.last() else { break };
        let changes: Vec<_> = out.into_iter().map(|(_, c)| c).collect();
        client.push(&room, &changes)?;
        store.outbox_sent(project_id, last).map_err(core)?;
        done.pushed += changes.len();
    }

    let members = client
        .rooms()
        .ok()
        .and_then(|rooms| rooms.into_iter().find(|r| r.name == room))
        .map(|r| r.members.into_iter().map(|m| (m.tag, m.user)).collect::<Vec<_>>())
        .unwrap_or_default();
    store.sync_succeeded(project_id, &members).map_err(core)?;
    Ok(done)
}
