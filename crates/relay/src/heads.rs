//! What every notification stream waits on: one poller for the whole relay.
//!
//! Each stream used to poll for itself -- open the database, check access, read its room's head,
//! once a second. That made an idle relay's work grow with its listeners: 900 streams cost 1.5
//! cores of a laptop doing nothing, and each held its own file descriptor against the
//! container's default limit of 1024 (note #333). One poller does the same reads once for all
//! of them, and publishes only when something changed, so an idle relay wakes no stream at all.
//!
//! It still polls, rather than relying on handlers to say what changed, for the reason
//! `docs/sync.md` gives: admin commands write from another process (`docker exec`), and a
//! forget or a revoked key must reach the streams too. A push pokes it, so a teammate's change
//! does not wait for the tick.

use crate::ServerDb;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify};

/// How often the poller looks when nothing poked it: what an admin command from another
/// process waits at most.
const POLL: Duration = Duration::from_secs(1);

/// What one poll saw.
#[derive(Debug)]
pub struct Snapshot {
    /// When the poll started. A stream that connected after this may be missing from
    /// `allowed` only because the poll read before its join was committed, so it is ended
    /// only by a snapshot taken after it connected.
    pub taken: Instant,
    /// Every room's head, by room id. A deleted room is missing.
    pub heads: HashMap<i64, i64>,
    /// (room id, key id) for every live membership whose key is live -- the access check every
    /// other route makes per request.
    pub allowed: HashSet<(i64, i64)>,
}

#[derive(Clone)]
pub struct Heads {
    tx: Arc<watch::Sender<Arc<Snapshot>>>,
    poke: Arc<Notify>,
    started: Arc<AtomicBool>,
}

impl Default for Heads {
    fn default() -> Self {
        let empty = Snapshot { taken: Instant::now(), heads: HashMap::new(), allowed: HashSet::new() };
        Heads { tx: Arc::new(watch::Sender::new(Arc::new(empty))), poke: Arc::default(), started: Arc::default() }
    }
}

impl Heads {
    /// Something was written: look now rather than at the next tick. A `Notify` keeps one
    /// permit, so a poke that lands while a poll runs is not lost.
    pub fn poke(&self) {
        self.poke.notify_one();
    }

    /// The snapshots, for one stream. Starts the poller with the first stream: it needs the
    /// runtime, which a `Relay` built outside one (tests, admin commands) does not have.
    pub fn subscribe(&self, db: &Arc<Mutex<ServerDb>>) -> watch::Receiver<Arc<Snapshot>> {
        let rx = self.tx.subscribe();
        if !self.started.swap(true, Ordering::SeqCst) {
            // Its own read-only connection per poll, so it never queues behind a push on the
            // shared handle. In-memory databases (tests) have no path and use the shared one.
            let path = db.lock().unwrap_or_else(|e| e.into_inner()).path().map(PathBuf::from);
            tokio::spawn(run(self.clone(), path, db.clone()));
        }
        rx
    }
}

async fn run(heads: Heads, path: Option<PathBuf>, db: Arc<Mutex<ServerDb>>) {
    loop {
        // Nobody listening: nothing to read for. The first subscriber pokes nothing, but it
        // already has its head from connecting, and the next tick serves it.
        if heads.tx.receiver_count() > 0 {
            let taken = Instant::now();
            let read = match &path {
                Some(p) => rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .ok()
                    .and_then(|c| read(&c, taken)),
                None => db.lock().ok().and_then(|d| read(&d.conn, taken)),
            };
            // A failed read publishes nothing: streams keep what they last saw.
            if let Some(next) = read {
                heads.tx.send_if_modified(|cur| {
                    let changed = cur.heads != next.heads || cur.allowed != next.allowed;
                    if changed {
                        *cur = Arc::new(next);
                    }
                    changed
                });
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = heads.poke.notified() => {}
        }
    }
}

fn read(conn: &rusqlite::Connection, taken: Instant) -> Option<Snapshot> {
    // One indexed MAX per room (`changes_room`), not a scan of the log.
    let mut st = conn.prepare("SELECT r.id, COALESCE((SELECT MAX(seq) FROM changes c WHERE c.room_id = r.id), 0) FROM rooms r").ok()?;
    let heads = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).ok()?.collect::<rusqlite::Result<HashMap<i64, i64>>>().ok()?;
    let mut st = conn
        .prepare(
            "SELECT m.room_id, k.id FROM members m JOIN keys k ON k.tag = m.tag
              WHERE m.left_at IS NULL AND k.revoked_at IS NULL",
        )
        .ok()?;
    let allowed = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).ok()?.collect::<rusqlite::Result<HashSet<(i64, i64)>>>().ok()?;
    Some(Snapshot { taken, heads, allowed })
}
