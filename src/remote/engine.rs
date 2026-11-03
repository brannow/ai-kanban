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
    claim(store, client);
    let hello = client.hello()?;
    if !(hello.protocol_min..=hello.protocol_max).contains(&protocol::PROTOCOL_VERSION) {
        let why = format!(
            "the relay speaks protocol {}-{}, this ai-kanban speaks {}. Upgrade ai-kanban (or the relay)",
            hello.protocol_min, hello.protocol_max, protocol::PROTOCOL_VERSION
        );
        pause(store, &why);
        return Err(RemoteError::Paused(why));
    }
    // The name first: on a key not used yet, it is what the tag is made from. Without one in the
    // plugin's settings, the name this machine already goes by: on a new relay's key it would
    // otherwise get a generated one, and the team would see a stranger after a move.
    let name = super::config::wanted_name().or_else(|| store.own_name().ok().flatten());
    let who = match client.identify(name.as_deref()) {
        Ok(who) => who,
        // The tag this machine publishes under belongs to another key on this relay: binding
        // this one to any other tag would split its work, so someone has to sort the keys out.
        Err(RemoteError::Conflict(why)) => {
            pause(store, &why);
            return Err(RemoteError::Paused(why));
        }
        Err(e) => return Err(e),
    };
    let _ = store.set_own_name(&who.user);
    let mut tag = store.origin_tag().ok().flatten();
    // A store with no tag has never published, so it holds no numbers under any tag: it takes
    // the key's, as `ai-kanban remote` would. That lets a key from the plugin's settings be saved
    // without a round trip at startup. A copied key is still caught by the high-water check below.
    //
    // So does a store whose boards are all disconnected: nothing it holds is in a room under
    // its old tag any more, which is when `set_origin_tag` allows the change (and why
    // `ai-kanban remote` makes it too). A new key from the settings menu then just works,
    // instead of pausing on a tag nothing depends on.
    if tag.as_deref() != Some(who.tag.as_str()) {
        // Someone else's tag, from a relay that handed tags out anew: taking it, or publishing
        // beside it, would merge their work into this store's. Said as that, not as a mismatch.
        if let Some(n) = store.tag_of_others(&who.tag).map_err(|e| RemoteError::Http(0, e.to_string()))? {
            let why = format!(
                "the relay gave this key tag {t}, which other people's work on this machine already carries ({n} task(s), \
                 notes or edits): taking it would make their work yours. The relay was probably set up anew and handed \
                 tags out in another order. Ask the relay admin for a new key, then `ai-kanban remote <url> --key <new key>`",
                t = who.tag
            );
            pause(store, &why);
            return Err(RemoteError::Paused(why));
        }
        if store.set_origin_tag(&who.tag).is_ok() {
            tag = Some(who.tag.clone());
        }
    }
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
            "this store is behind what the relay already has from {tag}, so its next tasks would reuse numbers the \
             room holds. Either this key is in use on another machine (every machine needs its own), or the store was \
             restored from a backup. Disconnect its boards (`ai-kanban room disconnect`), ask the relay admin for a \
             key of its own (`ai-kanban-relay key add <label>`), run `ai-kanban remote <url> --key <new key>`, then \
             connect again",
            tag = who.tag
        );
        pause(store, &why);
        return Err(RemoteError::Paused(why));
    }
    let core = |e: crate::core::Error| RemoteError::Http(0, e.to_string());
    let sent = store.confirmed_sent(&who.tag).map_err(core)?;
    match moved(store, client, &hello, sent, hw.outbox)? {
        // Another relay than the one this store synced with: moved, or set up anew. Everything
        // held goes there again, and the rooms are joined again (docs/sync.md, "Moving to
        // another relay"). For the person nothing changes.
        Some(true) => {
            let to = hello.instance.as_deref().unwrap_or_default();
            store.relay_moved(&who.tag, hw.outbox, to).map_err(core)?;
        }
        Some(false) => {
            // The other way round from the high-water check: the relay has less from this tag
            // than it confirmed receiving. It was restored from a snapshot, and lost what
            // arrived after it -- which every client has marked sent. Each machine sends its
            // own again; together the room is whole.
            if sent > hw.outbox {
                store.republish_after_relay_loss(&who.tag, hw.outbox).map_err(core)?;
            }
        }
        None => {
            let why = "this relay does not say which relay it is (an ai-kanban-relay from before 0.3.0), and this store \
                       last synced with one that does, so it cannot tell whether its rooms moved here. Nothing was sent. \
                       Upgrade the relay"
                .to_string();
            pause(store, &why);
            return Err(RemoteError::Paused(why));
        }
    }
    if let Some(instance) = &hello.instance {
        store.set_relay_instance(instance).map_err(core)?;
    }
    client.verified(store.relay_instance().map_err(core)?);
    let _ = store.set_paused(None);
    Ok(())
}

/// Whether this relay is another one than the store last synced with: `Some(true)` moved or set
/// up anew, `Some(false)` the same one, `None` cannot tell, which must stop sync.
///
/// Its instance id says it. A store that has never seen one -- every store from before 0.3.0,
/// on its first sync with a relay that names one -- tells from the relay itself: a relay that
/// has less from this store than it sent, or lacks a room this store is in under its id there,
/// is not the one the store synced with. On the same relay neither is true, and nothing happens
/// but recording the instance.
fn moved(store: &Store, client: &Client, hello: &protocol::Hello, sent: i64, relay_has: i64) -> Result<Option<bool>, RemoteError> {
    let core = |e: crate::core::Error| RemoteError::Http(0, e.to_string());
    let boards = store.connected_boards().map_err(core)?;
    let known = store.relay_instance().map_err(core)?;
    Ok(match (&hello.instance, known) {
        // Back to a relay this store moved away from. A session started before the move, still
        // holding the old URL and key, would read it as a new move and send everything back
        // there: refused, without pausing -- that process picks up the saved remote at its next
        // round. Only the saved remote being that relay again, a person's decision, goes back.
        (Some(now), Some(before)) if *now != before && store.left_relay(now).map_err(core)? => {
            let saved = super::RemoteConfig::load().ok().flatten();
            if saved.is_some_and(|cfg| !client.is_for(&cfg)) {
                return Err(RemoteError::BadRequest(
                    "this process still uses the relay this machine moved away from; it switches to the saved remote at its next round".into(),
                ));
            }
            Some(!boards.is_empty())
        }
        (Some(now), Some(before)) => Some(*now != before && !boards.is_empty()),
        (Some(_), None) if boards.is_empty() => Some(false),
        (Some(_), None) => {
            if sent > relay_has {
                Some(true)
            } else {
                let rooms = client.rooms()?;
                let mut gone = false;
                for (pid, room) in &boards {
                    if let Some(id) = store.relay_room(*pid).map_err(core)? {
                        gone |= !rooms.iter().any(|r| r.name == *room && r.id.is_none_or(|rid| rid == id));
                    }
                }
                Some(gone)
            }
        }
        (None, Some(_)) => None,
        (None, None) => Some(false),
    })
}

/// Joins `room` on a relay this store moved to (`Store::relay_moved`), creating it if nobody is
/// back yet. A room there that already holds content, from members none of whom this store has
/// ever had work from, is another team's that happens to share the name: left again, and sync
/// paused, rather than merging this board into it.
fn rejoin(store: &Store, client: &Client, project_id: i64, room: &str) -> Result<(), RemoteError> {
    let core = |e: crate::core::Error| RemoteError::Http(0, e.to_string());
    let board = store.project(project_id).map_err(core)?.name;
    let joined = client.join(room, Some(&board))?;
    if !joined.created && joined.room.changes > 0 {
        let known = store.tags_known_in(room).map_err(core)?;
        let own = store.own_tags().map_err(core)?;
        if !joined.room.members.iter().any(|m| !own.contains(&m.tag) && known.contains(&m.tag)) {
            let _ = client.leave(room);
            let why = format!(
                "room {room} on this relay already holds a board, and none of its members are people this board has                  work from: it is someone else's room under the same name. Nothing was sent. Ask the relay admin which                  room is your team's"
            );
            pause(store, &why);
            return Err(RemoteError::Paused(why));
        }
    }
    store.rejoined(project_id, joined.room.id).map_err(core)?;
    Ok(())
}

/// One last round with the relay this machine is switching away from, before the new one is
/// touched, so a change that reached only the old relay is not left behind when the team moves
/// (docs/sync.md, "Moving to another relay"). Bounded by `budget`; a relay that is gone -- set up
/// anew, not moved -- costs about one connect timeout, and has nothing to drain anyway.
///
/// Only from the relay this store is still on: once it has synced with the new one, a round
/// with the old would read as a move back there. Returns how many boards it drained.
pub fn drain(store: &Store, old: &super::RemoteConfig, budget: std::time::Duration) -> usize {
    let deadline = std::time::Instant::now() + budget;
    let c = Client::new(&old.url, &old.key, budget.min(std::time::Duration::from_secs(3)));
    claim(store, &c);
    let Ok(hello) = c.hello() else { return 0 };
    if let (Some(theirs), Ok(Some(ours))) = (hello.instance.as_deref(), store.relay_instance())
        && theirs != ours
    {
        return 0;
    }
    // Still the relay the store is on, so this is no move: the check only marks the client as
    // checked for it, which the bounded rounds below need.
    if check_identity(store, &c).is_err() {
        return 0;
    }
    let mut drained = 0;
    for (pid, _) in store.connected_boards().unwrap_or_default() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        if sync_board_within(store, &c, pid, left).is_ok() {
            drained += 1;
        }
    }
    drained
}

/// Makes `client` ask for this store's tag on every request (`Client::claim`). Called by every
/// entry point that talks to the relay with a store at hand, before its first request: any
/// request binds a key the relay has not bound yet, and one sent without the claim would bind
/// it to a generated tag for good.
pub fn claim(store: &Store, client: &Client) {
    client.claim(store.origin_tag().ok().flatten().as_deref());
}

fn pause(store: &Store, why: &str) {
    let _ = store.set_paused(Some(why));
}

/// One round for `project_id`: flush, push the outbox, pull and apply until caught up, and
/// record who is in the room. Returns what moved.
pub fn sync_board(store: &Store, client: &Client, project_id: i64) -> Result<Synced, RemoteError> {
    finish(store, project_id, round(store, client, project_id, None, true))
}

/// A round that only moves changes: push and pull, no member list. For the rounds a write or
/// a teammate's push sets off, whose whole point is getting that change across quickly; from
/// across the world every extra request is ~0.3s on the trip (note #333). The member list is
/// refreshed by the background round once a minute.
pub fn sync_board_changes(store: &Store, client: &Client, project_id: i64) -> Result<Synced, RemoteError> {
    finish(store, project_id, round(store, client, project_id, None, false))
}

/// A round that gives up starting new requests once `budget` is spent, and skips refreshing the
/// member list. For the round before a claim, which runs inside a tool call: the client's
/// timeout bounds each request, and a relay that accepts connections but never answers would
/// otherwise stall every one of the round's requests in turn. What it did not get to, the
/// background round does.
pub fn sync_board_within(store: &Store, client: &Client, project_id: i64, budget: std::time::Duration) -> Result<Synced, RemoteError> {
    let deadline = std::time::Instant::now() + budget;
    finish(store, project_id, round(store, client, project_id, Some(deadline), false))
}

/// The relay's "no room" and "not a member", said as what they mean for this board, and what
/// to do. The board itself keeps everything either way.
fn room_gone(room: &str, e: RemoteError) -> RemoteError {
    match e {
        RemoteError::NotFound(m) if m.contains("no room") => RemoteError::NotFound(format!(
            "room {room} no longer exists on the relay (it was deleted). This board keeps everything it has; \
             `ai-kanban room disconnect {room}` makes it private again"
        )),
        RemoteError::NotFound(m) if m.contains("not a member") => RemoteError::NotFound(format!(
            "this board is no longer a member of room {room}: the room was deleted and opened again, or this \
             machine left it. This board keeps everything it has; `ai-kanban room disconnect {room}` makes it \
             private again"
        )),
        e => e,
    }
}

fn finish(store: &Store, project_id: i64, result: Result<Synced, RemoteError>) -> Result<Synced, RemoteError> {
    match &result {
        // A pause is the store's state, reported once by the header, not as this board's error.
        Err(RemoteError::Paused(_)) | Ok(_) => {}
        // Sync cannot go on with this build: stop, and say so, until an update. The next
        // `check_identity` that passes lifts it.
        Err(e @ RemoteError::Upgrade(msg)) => {
            pause(store, msg);
            let _ = store.sync_failed(project_id, &e.recorded());
        }
        // A reused change id means this store was restored: stop until it has a new tag.
        Err(e @ RemoteError::Conflict(msg)) => {
            pause(store, msg);
            let _ = store.sync_failed(project_id, &e.recorded());
        }
        Err(e) => {
            let _ = store.sync_failed(project_id, &e.recorded());
        }
    }
    result
}

fn round(store: &Store, client: &Client, project_id: i64, deadline: Option<std::time::Instant>, members: bool) -> Result<Synced, RemoteError> {
    let core = |e: crate::core::Error| RemoteError::Http(0, e.to_string());
    claim(store, client);
    // With a deadline, no new request starts once it has passed. The round then ends early as
    // a success of what it did; it does not claim to be a full sync.
    let out_of_time = || deadline.is_some_and(|d| std::time::Instant::now() >= d);
    if let Some(why) = store.paused().map_err(core)? {
        return Err(RemoteError::Paused(why));
    }
    let room = store.room_of(project_id).map_err(core)?.ok_or_else(|| RemoteError::NotFound("this board is not connected to a room".into()))?;
    let mut done = Synced::default();
    // Nothing goes to a relay this client has not been checked against with the store where it
    // is now: it may be another relay than the store synced with -- or the store may have moved
    // since, through another process, while this one still holds the old relay. Only
    // `check_identity` finds that out, and refuses or prepares the rooms. The rounds set off by
    // a change or before a claim skip the check otherwise. The one before a claim does not run
    // it inside its budget: it skips the pull, and the background round checks.
    if !client.is_verified_for(store.relay_instance().map_err(core)?.as_deref()) {
        if deadline.is_some() {
            return Ok(done);
        }
        check_identity(store, client)?;
    }
    if store.needs_rejoin(project_id).map_err(core)? {
        rejoin(store, client, project_id, &room)?;
    }

    store.flush().map_err(core)?;
    loop {
        if out_of_time() {
            return Ok(done);
        }
        let out = store.outbox(project_id, MAX_BATCH).map_err(core)?;
        let Some(&(last, _)) = out.last() else { break };
        let changes: Vec<_> = out.into_iter().map(|(_, c)| c).collect();
        client.push(&room, &changes).map_err(|e| room_gone(&room, e))?;
        store.outbox_sent(project_id, last).map_err(core)?;
        done.pushed += changes.len();
    }

    loop {
        if out_of_time() {
            return Ok(done);
        }
        let since = store.sync_status(project_id).map_err(core)?.map(|s| s.cursor).unwrap_or(0);
        let page = client.pull(&room, since, MAX_PAGE).map_err(|e| room_gone(&room, e))?;
        // The room's log is shorter than what this board has read: the relay went back to a
        // snapshot, and its new changes reuse numbers this board counts as read. Read it again
        // from the start; what is already here is ignored.
        if page.head < since {
            store.rewind_cursor(project_id, 0).map_err(core)?;
            continue;
        }
        // The same name, but is it the same room? One deleted on the relay and opened again by
        // someone else has another id, and its tasks are not this board's -- checked before
        // anything is applied. A board connected before ids were kept takes this one.
        if let Some(id) = page.room {
            match store.relay_room(project_id).map_err(core)? {
                None => store.set_relay_room(project_id, id).map_err(core)?,
                Some(mine) if mine != id => {
                    return Err(RemoteError::NotFound(format!(
                        "room {room} was deleted on the relay and opened again by someone else: it is another room \
                         now. This board keeps everything it has; `ai-kanban room disconnect {room}` makes it private \
                         again, and `ai-kanban room connect {room}` would join the new one"
                    )));
                }
                Some(_) => {}
            }
        }
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
        if out_of_time() {
            return Ok(done);
        }
        let out = store.outbox(project_id, MAX_BATCH).map_err(core)?;
        let Some(&(last, _)) = out.last() else { break };
        let changes: Vec<_> = out.into_iter().map(|(_, c)| c).collect();
        client.push(&room, &changes).map_err(|e| room_gone(&room, e))?;
        store.outbox_sent(project_id, last).map_err(core)?;
        done.pushed += changes.len();
    }

    if deadline.is_some() {
        // The quick round leaves the member list to the background one: it is one more request,
        // and the header can show the team from a minute ago.
        return Ok(done);
    }
    if !members {
        store.sync_succeeded_keeping_members(project_id).map_err(core)?;
        return Ok(done);
    }
    match client.rooms() {
        Ok(rooms) => {
            let members = rooms
                .into_iter()
                .find(|r| r.name == room)
                .map(|r| r.members.into_iter().map(|m| (m.tag, m.user)).collect::<Vec<_>>())
                .unwrap_or_default();
            store.sync_succeeded(project_id, &members).map_err(core)?;
        }
        // The changes moved; a failed lookup only means the team shown is the last one known,
        // not that the room is empty.
        Err(_) => store.sync_succeeded_keeping_members(project_id).map_err(core)?,
    }
    Ok(done)
}
