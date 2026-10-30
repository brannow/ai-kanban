//! The relay a team shares boards through. `docs/sync.md` is the design; the binary is
//! `ai-kanban-relay`.
//!
//! # Why it is not `serve`
//!
//! `serve` is a loopback-only window onto one person's store, with no auth, guarded against
//! the person's own browser by a Host/Origin check. The relay is the opposite on every axis:
//! reached over the network behind a proxy, where `Host` is the public name, by programs
//! rather than browsers, and every request carries a key. Sharing a router would mean every
//! route answering to two security models.
//!
//! # What it does not do
//!
//! Run board logic. It authenticates, orders changes and hands out "everything since N". So
//! it seldom needs upgrading in step with clients.

pub mod admin;
pub mod debug;
pub mod heads;
pub mod pages;
pub mod store;

use ai_kanban_protocol::{self as protocol, Hello, Push, WhoAmI};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde::Deserialize;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
pub use store::{Principal, ServerDb, ServerError};

/// A push is at most `MAX_BATCH` changes. Bodies are prose-sized, so 8 MiB is generous for a
/// full batch and still refuses a client gone wrong before it is parsed.
pub(crate) const BODY_LIMIT: usize = 8 * 1024 * 1024;

/// Empty rooms are removed after this long.
pub const EMPTY_ROOM_AGE: i64 = 7 * 24 * 3600;

/// The share of the open-file limit notification streams may hold, in percent. Each stream is
/// one open connection for as long as a session or page runs; at the limit the relay could not
/// accept anything, pushes and pulls included, and stayed that way (scripts/relay-load). The
/// rest is for those requests, the database and the proxy's pooled connections.
pub const STREAM_SHARE: usize = 85;
/// What a refused stream is told to wait. Clients back off on their own anyway.
const BUSY_RETRY_AFTER: &str = "60";

#[derive(Clone)]
pub struct Relay {
    pub db: Arc<Mutex<ServerDb>>,
    /// Set by `--debug`: where every request and stream event is written (`debug.rs`).
    pub trace: Option<debug::Sink>,
    /// Every room's head and who may see it, polled once for all notification streams
    /// (`heads.rs`).
    heads: heads::Heads,
    streams: Arc<Streams>,
}

/// How many notification streams are open, and how many may be.
#[derive(Default)]
struct Streams {
    open: AtomicUsize,
    max: Option<usize>,
    /// Whether the last stream that asked was refused: the log says so once per stretch, not
    /// once per client retrying.
    refusing: AtomicBool,
}

/// One open stream, counted for as long as it lives.
struct OpenStream(Arc<Streams>);

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Streams {
    fn open(self: &Arc<Self>) -> Result<OpenStream, ServerError> {
        let n = self.open.fetch_add(1, Ordering::Relaxed) + 1;
        let guard = OpenStream(self.clone());
        match self.max {
            Some(max) if n > max => {
                drop(guard);
                if !self.refusing.swap(true, Ordering::Relaxed) {
                    eprintln!("busy: {max} notification streams open, the most allowed; refusing new ones (503) until some close");
                }
                // Degraded, not down: a refused client still syncs, on its own writes and once a
                // minute, and asks for a stream again after a backoff.
                Err(ServerError::Busy(format!("the relay has {max} notification streams open, its most; try again later")))
            }
            _ => {
                if self.refusing.swap(false, Ordering::Relaxed) {
                    println!("busy no more: accepting notification streams again");
                }
                Ok(guard)
            }
        }
    }
}

impl Relay {
    pub fn new(db: ServerDb) -> Self {
        Self { db: Arc::new(Mutex::new(db)), trace: None, heads: heads::Heads::default(), streams: Arc::default() }
    }

    /// At most `max` notification streams open at once; new ones above it are refused (503).
    pub fn with_max_streams(mut self, max: usize) -> Self {
        self.streams = Arc::new(Streams { max: Some(max), ..Streams::default() });
        self
    }

    /// Notification streams open right now.
    pub fn open_streams(&self) -> usize {
        self.streams.open.load(Ordering::Relaxed)
    }

    pub fn with_trace(mut self, sink: debug::Sink) -> Self {
        self.trace = Some(sink);
        self
    }

    fn db(&self) -> std::sync::MutexGuard<'_, ServerDb> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn auth(&self, headers: &HeaderMap) -> Result<Principal, ServerError> {
        let agent = client_check(headers)?;
        let key = bearer(headers).ok_or(ServerError::Unauthorized)?;
        let db = self.db();
        let p = db.authenticate(key)?;
        if let Some(agent) = agent {
            db.saw_client(p.key_id, agent)?;
        }
        Ok(p)
    }
}

/// The client's `User-Agent`, refused if it says it speaks a protocol older than this relay
/// accepts. Clients check `hello` themselves and pause first; this is for one that does not,
/// so the relay does not have to trust that every client does. One that does not say what it
/// is (an older ai-kanban, another program) passes: there is nothing to refuse it on.
fn client_check(headers: &HeaderMap) -> Result<Option<&str>, ServerError> {
    let Some(agent) = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) else { return Ok(None) };
    if let Some(c) = protocol::parse_client_agent(agent) {
        if c.protocol < protocol::MIN_PROTOCOL_VERSION {
            return Err(ServerError::UpgradeRequired(format!(
                "this relay needs ai-kanban speaking protocol {} or newer; this is ai-kanban {} (protocol {}). \
                 Update it: /plugin -> ai-kanban -> update, or the binary you run",
                protocol::MIN_PROTOCOL_VERSION, c.version, c.protocol
            )));
        }
    }
    Ok(Some(agent))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        // The scheme is case-insensitive (RFC 9110), so `bearer` from a hand-written client
        // must work as well as `Bearer`.
        .and_then(|v| v.split_once(' ').filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer")).map(|(_, k)| k))
}

pub fn router(relay: Relay) -> Router {
    let routes = Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/whoami", get(whoami))
        .route("/v1/me", axum::routing::put(set_name))
        .route("/v1/rooms", get(rooms))
        .route("/v1/rooms/{room}/join", post(join))
        .route("/v1/rooms/{room}/leave", post(leave))
        .route("/v1/rooms/{room}/changes", get(pull).post(push))
        .route("/v1/rooms/{room}/events", get(events))
        .layer(DefaultBodyLimit::max(BODY_LIMIT));
    // Outermost, so it also sees what the body limit and the extractors turn away.
    let routes = match relay.trace {
        Some(_) => routes.layer(middleware::from_fn_with_state(relay.clone(), debug::trace)),
        None => routes,
    };
    routes.with_state(relay)
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ServerError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ServerError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ServerError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ServerError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ServerError::UpgradeRequired(_) => (StatusCode::UPGRADE_REQUIRED, "upgrade_required"),
            ServerError::Busy(_) => (StatusCode::SERVICE_UNAVAILABLE, "busy"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "error"),
        };
        let body = serde_json::json!({ "error": { "code": code, "message": self.to_string() } });
        let mut res = (status, Json(body)).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            res.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from_static(BUSY_RETRY_AFTER));
            // Kept alive, the refused connection would go on holding the open file the refusal
            // is there to save.
            res.headers_mut().insert(header::CONNECTION, header::HeaderValue::from_static("close"));
        }
        res
    }
}

type ApiResult<T> = Result<Json<T>, ServerError>;

/// The one route without a key: a client has to learn whether it can talk to this server at
/// all before it has anything worth authenticating.
async fn hello() -> Json<Hello> {
    Json(Hello {
        protocol_min: protocol::MIN_PROTOCOL_VERSION,
        protocol_max: protocol::PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn whoami(State(r): State<Relay>, headers: HeaderMap) -> ApiResult<WhoAmI> {
    let p = r.auth(&headers)?;
    let high_water = r.db().high_water(&p.tag)?;
    Ok(Json(WhoAmI { user: p.user, tag: p.tag, high_water }))
}

/// The name this key's machine goes by, set by its client. The first call fixes the key's tag
/// from it, which is why it reads the key without `auth`: that would give an unnamed key a
/// generated name, and its tag, first.
async fn set_name(State(r): State<Relay>, headers: HeaderMap, Json(body): Json<protocol::SetName>) -> ApiResult<WhoAmI> {
    let agent = client_check(&headers)?;
    let key = bearer(&headers).ok_or(ServerError::Unauthorized)?;
    let db = r.db();
    let (key_id, _) = db.lookup(key)?;
    let p = db.name_key(key_id, &body.name)?;
    if let Some(agent) = agent {
        db.saw_client(key_id, agent)?;
    }
    let high_water = db.high_water(&p.tag)?;
    Ok(Json(WhoAmI { user: p.user, tag: p.tag, high_water }))
}

async fn rooms(State(r): State<Relay>, headers: HeaderMap) -> ApiResult<Vec<protocol::RoomInfo>> {
    r.auth(&headers)?;
    Ok(Json(r.db().rooms()?))
}

#[derive(Deserialize)]
struct JoinQuery {
    /// The joining board's name, kept if this join creates the room.
    board: Option<String>,
}

async fn join(
    State(r): State<Relay>,
    headers: HeaderMap,
    Path(room): Path<String>,
    Query(q): Query<JoinQuery>,
) -> ApiResult<protocol::Joined> {
    let p = r.auth(&headers)?;
    Ok(Json(r.db().join_from(&room, &p, q.board.as_deref())?))
}

async fn leave(State(r): State<Relay>, headers: HeaderMap, Path(room): Path<String>) -> Result<StatusCode, ServerError> {
    let p = r.auth(&headers)?;
    r.db().leave(&room, &p)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn push(
    State(r): State<Relay>,
    headers: HeaderMap,
    Path(room): Path<String>,
    Json(body): Json<Push>,
) -> ApiResult<protocol::Pushed> {
    let p = r.auth(&headers)?;
    let pushed = r.db().push(&room, &p, &body.changes)?;
    // After the commit, so the poll it sets off already sees the new head. Without it the
    // change waited for the next tick: up to a second of every change's trip (note #333).
    r.heads.poke();
    Ok(Json(pushed))
}

#[derive(Deserialize)]
struct PullQuery {
    #[serde(default)]
    since: i64,
    limit: Option<usize>,
}

async fn pull(
    State(r): State<Relay>,
    headers: HeaderMap,
    Path(room): Path<String>,
    Query(q): Query<PullQuery>,
) -> ApiResult<protocol::Page> {
    let p = r.auth(&headers)?;
    Ok(Json(r.db().pull(&room, &p, q.since, q.limit.unwrap_or(protocol::MAX_PAGE))?))
}

/// "Room changed, head N" -- never the changes themselves. The client pulls from its own
/// cursor, which is what makes a dropped stream harmless: a missed notification only delays a
/// pull. The first event is the current head, so a client that reconnects learns at once
/// whether it missed anything.
async fn events(
    State(r): State<Relay>,
    headers: HeaderMap,
    Path(room): Path<String>,
    traced: Option<Extension<debug::Traced>>,
) -> Result<impl IntoResponse, ServerError> {
    let p = r.auth(&headers)?;
    let (room_id, head) = r.db().member_head(&room, &p)?;
    let open = r.streams.open()?;
    let log = r.trace.clone().zip(traced).map(|(sink, Extension(t))| debug::StreamLog::new(sink, t.0, &format!("{}/{}", p.user, p.tag), &room));
    let key_id = p.key_id;
    let mut snapshots = r.heads.subscribe(&r.db);
    let connected = std::time::Instant::now();
    let stream = async_stream::stream! {
        // Moved in, so they are dropped with the stream: the log records the close, and the
        // count goes down.
        let mut log = log;
        let _open = open;
        let mut last = head;
        if let Some(l) = &log { l.head(last) }
        yield Ok::<_, Infallible>(SseEvent::default().event("head").data(last.to_string()));
        // Woken only when a poll saw something change, in any room: checking ours is a lookup.
        while snapshots.changed().await.is_ok() {
            let snap = snapshots.borrow_and_update().clone();
            // Access is checked on every change, like every other route checks it per request:
            // a stream must not outlive a revoked key, a left room or a deleted one. Only on a
            // poll that started after this stream connected, though; an older one may predate
            // the join this stream rests on.
            let now = snap.heads.get(&room_id).copied().filter(|_| snap.allowed.contains(&(room_id, key_id)));
            let Some(now) = now else {
                if snap.taken < connected {
                    continue;
                }
                // The client's next request says which.
                if let Some(l) = &mut log { l.why = "access gone (room, membership, key or user)" }
                break;
            };
            if now != last {
                last = now;
                if let Some(l) = &log { l.head(last) }
                yield Ok(SseEvent::default().event("head").data(last.to_string()));
            }
        }
    };
    // The keep-alive comment doubles as the heartbeat the design promises: it keeps proxies
    // from cutting an idle stream, and its absence tells the client the stream is dead.
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30))))
}

/// How the relay runs.
pub struct Options {
    pub listen: std::net::SocketAddr,
    pub db: PathBuf,
    /// Seconds between snapshots, or none.
    pub snapshot_every: Option<u64>,
    pub snapshot_dir: PathBuf,
    pub keep: usize,
    /// Log every request and stream event (`debug.rs`).
    pub debug: bool,
    /// Notification streams allowed at once. None: `STREAM_SHARE` of the open-file limit,
    /// where the system says what it is.
    pub max_streams: Option<usize>,
}

/// The soft limit on open files, where it can be read without a dependency: Linux, which is
/// where the relay runs (its container).
pub fn open_file_limit(pid: &str) -> Option<usize> {
    let limits = std::fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    line.split_whitespace().nth(3)?.parse().ok()
}

pub async fn serve(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let db = ServerDb::open(&opts.db)?;
    let mut relay = Relay::new(db);
    let limit = open_file_limit("self");
    let max_streams = opts.max_streams.or(limit.map(|l| l * STREAM_SHARE / 100));
    if let Some(max) = max_streams {
        relay = relay.with_max_streams(max);
    }
    if opts.debug {
        relay = relay.with_trace(debug::stdout());
    }

    spawn_housekeeping(relay.clone(), opts.snapshot_every, opts.snapshot_dir.clone(), opts.keep);

    let listener = tokio::net::TcpListener::bind(opts.listen).await?;
    println!("ai-kanban relay: http://{} (protocol {})", opts.listen, protocol::PROTOCOL_VERSION);
    println!("database: {}", opts.db.display());
    match opts.snapshot_every {
        Some(s) => println!("snapshots: every {}s into {}, keeping {}", s, opts.snapshot_dir.display(), opts.keep),
        None => println!("snapshots: off (pass --snapshot-every)"),
    }
    match (max_streams, limit) {
        (Some(m), _) if opts.max_streams.is_some() => println!("streams: at most {m} (--max-streams)"),
        (Some(m), Some(l)) => println!("streams: at most {m} ({STREAM_SHARE}% of the open-file limit, {l})"),
        _ => println!("streams: no limit known (pass --max-streams)"),
    }
    if opts.debug {
        println!("debug: logging every request, response and stream event, bodies included -- turn it off when done");
    }
    axum::serve(listener, router(relay)).await?;
    Ok(())
}

/// Empty-room removal hourly, and snapshots when asked for. Failures are printed and survived:
/// a missed snapshot must not take the relay down with it.
fn spawn_housekeeping(relay: Relay, snapshot_every: Option<u64>, dir: PathBuf, keep: usize) {
    tokio::spawn(async move {
        let mut last_snapshot = std::time::Instant::now();
        loop {
            match relay.db().remove_empty_rooms(EMPTY_ROOM_AGE) {
                Ok(gone) if !gone.is_empty() => println!("removed empty rooms: {}", gone.join(", ")),
                Ok(_) => {}
                Err(e) => eprintln!("empty-room cleanup failed: {e}"),
            }
            if let Some(every) = snapshot_every {
                if last_snapshot.elapsed() >= Duration::from_secs(every) {
                    last_snapshot = std::time::Instant::now();
                    match take_snapshot(&relay.db(), &dir, keep) {
                        Ok(p) => println!("snapshot: {}", p.display()),
                        Err(e) => eprintln!("snapshot failed: {e}"),
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

/// Writes `server-<unix seconds>.db` into `dir` and prunes all but the newest `keep`.
pub fn take_snapshot(db: &ServerDb, dir: &std::path::Path, keep: usize) -> Result<PathBuf, ServerError> {
    let dest = dir.join(format!("server-{}.db", store::now()));
    db.snapshot(&dest)?;
    let mut snaps: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| {
            let n = p.file_name()?.to_str()?.strip_prefix("server-")?.strip_suffix(".db")?.parse().ok()?;
            Some((n, p))
        })
        .collect();
    // By the number, not the name: sorting names only works while every timestamp has the same
    // width, and a snapshot copied back in by hand under another name would get the newest one
    // pruned instead of the oldest.
    snaps.sort();
    let snaps: Vec<PathBuf> = snaps.into_iter().map(|(_, p)| p).collect();
    let excess = snaps.len().saturating_sub(keep.max(1));
    for old in snaps.into_iter().take(excess) {
        std::fs::remove_file(old)?;
    }
    Ok(dest)
}
