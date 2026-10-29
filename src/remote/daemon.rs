//! Keeping shared boards in sync while an agent session or the web UI runs: inside the MCP
//! process, and inside `serve`.
//!
//! Not a daemon of its own -- the vision rules one out -- but a thread in a process that runs
//! anyway. It syncs every connected board at startup, when a teammate's push is
//! announced on a room's notification stream, shortly after this session writes, and once a
//! minute regardless. The stream only says *that* something changed; the round pulls from its
//! own cursor, so a missed notification costs a delay, never a change.
//!
//! Nothing here may write to stdout: in the MCP process stdout is the protocol.

use super::client::{Client, RemoteError};
use super::config::{self, RemoteConfig};
use super::engine::{check_identity, sync_board, sync_board_within};
use crate::core::Store;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The backstop interval: what a session without working notifications still gets.
const EVERY: Duration = Duration::from_secs(60);
/// Writes in quick succession (a task_update and its log) go out as one round.
const SETTLE: Duration = Duration::from_millis(300);
/// The round before a claim runs inside a tool call. No request starts after this budget is
/// spent, and each is bounded by `CLAIM_REQUEST`, so an unreachable or stalled relay costs the
/// agent about this much at most, and the claim goes through regardless.
const CLAIM_BUDGET: Duration = Duration::from_millis(2500);
const CLAIM_REQUEST: Duration = Duration::from_millis(1200);
/// How long a stream counts as live after the last thing it sent. The relay sends a heartbeat
/// every 30s, so a stream silent for longer than this is dead even if the socket has not noticed.
const LIVE_FOR: Duration = Duration::from_secs(40);

/// What each room's notification stream last said: the head it announced, and when it last
/// sent anything at all. An entry exists only while its stream is connected.
type Live = Arc<Mutex<HashMap<String, (i64, Instant)>>>;

pub struct Syncer {
    kick: Sender<()>,
    claim: Client,
    live: Live,
}

impl Syncer {
    /// Starts syncing in the background if this run may use the network and a remote is set.
    /// `None` otherwise, and then nothing in the session touches the network.
    pub fn start() -> Option<Syncer> {
        if !config::sync_enabled() {
            return None;
        }
        let cfg = RemoteConfig::load().ok().flatten()?;
        let (tx, rx) = channel::<()>();
        let background = Client::new(&cfg.url, &cfg.key, Duration::from_secs(30));
        let notify = tx.clone();
        let live: Live = Arc::default();
        let watching = live.clone();
        std::thread::Builder::new()
            .name("ai-kanban-sync".into())
            .spawn(move || {
                // Its own connection: the tool handlers' one sits behind a mutex, and a round
                // waiting on the network must not hold it.
                let Ok(store) = Store::open_default() else { return };
                let mut watched = HashSet::new();
                let mut named = false;
                let mut backoff = EVERY;
                loop {
                    let healthy = round(&store, &background, &mut watched, &mut named, &notify, &watching);
                    // A store that cannot sync (offline, paused) is checked less and less often,
                    // up to every five minutes, instead of hammering a relay that is not there.
                    backoff = if healthy { EVERY } else { (backoff * 2).min(Duration::from_secs(300)) };
                    match rx.recv_timeout(if healthy { EVERY } else { backoff }) {
                        Ok(()) => {
                            std::thread::sleep(SETTLE);
                            while rx.try_recv().is_ok() {}
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok()?;
        Some(Syncer { kick: tx, claim: Client::new(&cfg.url, &cfg.key, CLAIM_REQUEST), live })
    }

    /// Something was written: publish it soon.
    pub fn kick(&self) {
        let _ = self.kick.send(());
    }

    /// A quick round for one board, on the caller's connection, before an agent claims work or
    /// files a task -- so what it sees is current, not as of the last minute. Failures are
    /// recorded for the header and otherwise ignored: the claim must never fail because the
    /// relay is unreachable.
    ///
    /// Skipped when the room's stream is live and everything it announced is already pulled:
    /// the local copy is then as current as a round would make it, and the round would only
    /// cost the agent two trips to the relay (0.6s from across the world). What this misses is a
    /// teammate's change from the last moment before the relay announces it, about two seconds.
    pub fn now(&self, store: &Store, project_id: i64) {
        let Some((room, cursor)) = store.room_cursor(project_id).ok().flatten() else { return };
        if store.paused().ok().flatten().is_some() || self.current(&room, cursor) {
            return;
        }
        let _ = sync_board_within(store, &self.claim, project_id, CLAIM_BUDGET);
    }

    fn current(&self, room: &str, cursor: i64) -> bool {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.get(room).is_some_and(|(head, seen)| seen.elapsed() < LIVE_FOR && cursor >= *head)
    }
}

/// One background round over every connected board. Returns whether the relay was reachable and
/// sync is allowed.
fn round(store: &Store, client: &Client, watched: &mut HashSet<String>, named: &mut bool, notify: &Sender<()>, live: &Live) -> bool {
    let boards = store.connected_boards().unwrap_or_default();
    if boards.is_empty() {
        // Nothing to sync yet, but a name from the plugin's settings has to reach the relay
        // before anything else does: the first `room connect` runs in a shell that does not
        // have the setting, and a key used without a name fixes its tag from a generated one.
        // Until the relay has had it once; the rounds with boards send it every time.
        if !*named && config::wanted_name().is_some() {
            *named = check_identity(store, client).is_ok();
        }
        return true;
    }
    // Local and cheap, so first: even when the relay is unreachable, pending writes become
    // countable changes, and the header can say how many are waiting.
    let _ = store.flush();
    match check_identity(store, client) {
        Ok(()) => {}
        Err(RemoteError::Paused(_)) => return false,
        Err(e) => {
            for (pid, _) in &boards {
                let _ = store.sync_failed(*pid, &e.recorded());
            }
            return false;
        }
    }
    let mut ok = true;
    for (pid, room) in boards {
        if sync_board(store, client, pid).is_err() {
            ok = false;
        }
        if watched.insert(room.clone()) {
            watch(client.clone(), room, notify.clone(), live.clone());
        }
    }
    ok
}

/// Follows one room's notification stream for the life of the process, reconnecting with
/// backoff, and turns every announced head into a round.
fn watch(client: Client, room: String, notify: Sender<()>, live: Live) {
    let _ = std::thread::Builder::new().name(format!("ai-kanban-watch-{room}")).spawn(move || {
        let mut backoff = Duration::from_secs(5);
        loop {
            let started = Instant::now();
            let mut alive = true;
            let _ = client.watch(&room, |head| {
                let mut l = live.lock().unwrap_or_else(|e| e.into_inner());
                match head {
                    Some(h) => {
                        l.insert(room.clone(), (h, Instant::now()));
                        alive = notify.send(()).is_ok();
                    }
                    // A heartbeat: still connected. Before the first head there is nothing to
                    // vouch for yet.
                    None => {
                        if let Some(e) = l.get_mut(&room) {
                            e.1 = Instant::now();
                        }
                    }
                }
                alive
            });
            // Disconnected: until it is back, a claim catches up the slow way.
            live.lock().unwrap_or_else(|e| e.into_inner()).remove(&room);
            if !alive {
                return;
            }
            // A stream that stayed up a while was fine; one that drops at once is backed off.
            backoff = if started.elapsed() > Duration::from_secs(60) { Duration::from_secs(5) } else { (backoff * 2).min(Duration::from_secs(300)) };
            std::thread::sleep(backoff);
        }
    });
}
