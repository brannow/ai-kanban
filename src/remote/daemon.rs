//! Keeping shared boards in sync while an agent session runs, inside the MCP process.
//!
//! Not a daemon of its own -- the vision rules one out -- but a thread in the process every
//! session already has. It syncs every connected board at startup, when a teammate's push is
//! announced on a room's notification stream, shortly after this session writes, and once a
//! minute regardless. The stream only says *that* something changed; the round pulls from its
//! own cursor, so a missed notification costs a delay, never a change.
//!
//! Nothing here may write to stdout: in the MCP process stdout is the protocol.

use super::client::{Client, RemoteError};
use super::config::{self, RemoteConfig};
use super::engine::{check_identity, sync_board, sync_board_within};
use crate::core::Store;
use std::collections::HashSet;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::Duration;

/// The backstop interval: what a session without working notifications still gets.
const EVERY: Duration = Duration::from_secs(60);
/// Writes in quick succession (a task_update and its log) go out as one round.
const SETTLE: Duration = Duration::from_millis(300);
/// The round before a claim runs inside a tool call. No request starts after this budget is
/// spent, and each is bounded by `CLAIM_REQUEST`, so an unreachable or stalled relay costs the
/// agent about this much at most, and the claim goes through regardless.
const CLAIM_BUDGET: Duration = Duration::from_millis(2500);
const CLAIM_REQUEST: Duration = Duration::from_millis(1200);

pub struct Syncer {
    kick: Sender<()>,
    claim: Client,
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
        std::thread::Builder::new()
            .name("ai-kanban-sync".into())
            .spawn(move || {
                // Its own connection: the tool handlers' one sits behind a mutex, and a round
                // waiting on the network must not hold it.
                let Ok(store) = Store::open_default() else { return };
                let mut watched = HashSet::new();
                let mut backoff = EVERY;
                loop {
                    let healthy = round(&store, &background, &mut watched, &notify);
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
        Some(Syncer { kick: tx, claim: Client::new(&cfg.url, &cfg.key, CLAIM_REQUEST) })
    }

    /// Something was written: publish it soon.
    pub fn kick(&self) {
        let _ = self.kick.send(());
    }

    /// A quick round for one board, on the caller's connection, before an agent claims work or
    /// files a task -- so what it sees is current, not as of the last minute. Failures are
    /// recorded for the header and otherwise ignored: the claim must never fail because the
    /// relay is unreachable.
    pub fn now(&self, store: &Store, project_id: i64) {
        if store.room_of(project_id).ok().flatten().is_some() && store.paused().ok().flatten().is_none() {
            let _ = sync_board_within(store, &self.claim, project_id, CLAIM_BUDGET);
        }
    }
}

/// One background round over every connected board. Returns whether the relay was reachable and
/// sync is allowed.
fn round(store: &Store, client: &Client, watched: &mut HashSet<String>, notify: &Sender<()>) -> bool {
    let boards = store.connected_boards().unwrap_or_default();
    if boards.is_empty() {
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
            watch(client.clone(), room, notify.clone());
        }
    }
    ok
}

/// Follows one room's notification stream for the life of the process, reconnecting with
/// backoff, and turns every announced head into a round.
fn watch(client: Client, room: String, notify: Sender<()>) {
    let _ = std::thread::Builder::new().name(format!("ai-kanban-watch-{room}")).spawn(move || {
        let mut backoff = Duration::from_secs(5);
        loop {
            let started = std::time::Instant::now();
            let mut alive = true;
            let _ = client.watch(&room, |_| {
                alive = notify.send(()).is_ok();
                alive
            });
            if !alive {
                return;
            }
            // A stream that stayed up a while was fine; one that drops at once is backed off.
            backoff = if started.elapsed() > Duration::from_secs(60) { Duration::from_secs(5) } else { (backoff * 2).min(Duration::from_secs(300)) };
            std::thread::sleep(backoff);
        }
    });
}
