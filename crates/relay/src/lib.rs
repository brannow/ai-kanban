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
use std::sync::{Arc, Mutex};
use std::time::Duration;
pub use store::{Principal, ServerDb, ServerError};

/// A push is at most `MAX_BATCH` changes. Bodies are prose-sized, so 8 MiB is generous for a
/// full batch and still refuses a client gone wrong before it is parsed.
pub(crate) const BODY_LIMIT: usize = 8 * 1024 * 1024;

/// How often each notification stream checks its room when nothing woke it. A push to this
/// relay wakes every stream at once (`Relay::pushed`); the tick is for what no handler sees:
/// `docker exec` admin commands write from another process. One indexed `MAX(seq)` per second
/// is the cheapest thing that sees those.
const POLL: Duration = Duration::from_secs(1);

/// Empty rooms are removed after this long.
pub const EMPTY_ROOM_AGE: i64 = 7 * 24 * 3600;

#[derive(Clone)]
pub struct Relay {
    pub db: Arc<Mutex<ServerDb>>,
    /// Set by `--debug`: where every request and stream event is written (`debug.rs`).
    pub trace: Option<debug::Sink>,
    /// Bumped after every push, so a stream looks at its room now rather than at its next tick:
    /// for clients far from the relay that tick was up to a second of every change's trip
    /// (note #333). A counter in a `watch` rather than a `Notify`, because a stream busy with
    /// its query when the push lands still sees the bump on its next wait.
    pushed: Arc<tokio::sync::watch::Sender<u64>>,
}

impl Relay {
    pub fn new(db: ServerDb) -> Self {
        Self { db: Arc::new(Mutex::new(db)), trace: None, pushed: Arc::new(tokio::sync::watch::Sender::new(0)) }
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
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "error"),
        };
        let body = serde_json::json!({ "error": { "code": code, "message": self.to_string() } });
        (status, Json(body)).into_response()
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
    // After the commit, so a woken stream's own connection already sees the new head.
    r.pushed.send_modify(|n| *n = n.wrapping_add(1));
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
    let log = r.trace.clone().zip(traced).map(|(sink, Extension(t))| debug::StreamLog::new(sink, t.0, &format!("{}/{}", p.user, p.tag), &room));
    let (tag, key_id) = (p.tag.clone(), p.key_id);
    // Its own connection, opened per tick, like `http::stream`: a reader polling through the
    // shared mutex would contend with every push. In-memory databases (tests) have no path, so
    // they fall back to the shared handle.
    let path: Option<PathBuf> = r.db().path().map(PathBuf::from);
    let db = r.db.clone();
    let mut pushed = r.pushed.subscribe();
    let stream = async_stream::stream! {
        // Moved in, so it is dropped -- and logs the close -- with the stream.
        let mut log = log;
        let mut last = head;
        if let Some(l) = &log { l.head(last) }
        yield Ok::<_, Infallible>(SseEvent::default().event("head").data(last.to_string()));
        loop {
            // A push to any room wakes every stream; each checks only its own, so a wake for
            // another room costs one query and sends nothing. The relay keeps the sender for
            // its whole life, so `changed` does not fail; if it did, the pattern would not
            // match and the tick alone would go on.
            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
                Ok(()) = pushed.changed() => {}
            }
            let now = match &path {
                Some(path) => rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .ok()
                    .and_then(|c| head_if_allowed(&c, room_id, &tag, key_id)),
                None => db.lock().ok().and_then(|d| head_if_allowed(&d.conn, room_id, &tag, key_id)),
            };
            // None means the room was deleted, or the key, the user or the membership is gone:
            // end the stream, and the client's next request says which.
            let Some(now) = now else {
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

/// The room's head, if the stream may still see it. Checked on every tick rather than once at
/// connect, because every other route checks access per request: a stream that outlived a
/// revoked key or a left room would be the one route that doesn't.
fn head_if_allowed(conn: &rusqlite::Connection, room_id: i64, tag: &str, key_id: i64) -> Option<i64> {
    let allowed: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM members m
                             JOIN keys k ON k.tag = m.tag
                            WHERE m.room_id = ?1 AND m.tag = ?2 AND m.left_at IS NULL
                              AND k.id = ?3 AND k.revoked_at IS NULL)",
            rusqlite::params![room_id, tag, key_id],
            |r| r.get(0),
        )
        .ok()?;
    if !allowed {
        return None;
    }
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes WHERE room_id = ?1", [room_id], |r| r.get(0))
        .ok()
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
}

pub async fn serve(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let db = ServerDb::open(&opts.db)?;
    let mut relay = Relay::new(db);
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
