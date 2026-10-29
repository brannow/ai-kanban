//! `--debug`: every request, every response and every event-stream message, one line each, so
//! what clients really send and receive can be watched live (`./kanban logs -f`).
//!
//! Off by default, and not for a relay left running: it prints task and note bodies, so anyone
//! who can read the container's logs reads the boards. The key is never printed. A request
//! shows the user and tag it authenticated as, or the last four characters of a key that did
//! not authenticate.
//!
//! A middleware rather than lines in the handlers, because a request the router turns away
//! before any handler runs (no key, a body that is not JSON) is exactly the one worth seeing.
//! The event stream is the exception: its response never ends, so it is passed through
//! unbuffered, and `events()` logs what it sends under the same request number. The 30s
//! keep-alive comments are written by axum, not by the stream, and do not show.
//!
//! Line format: `HH:MM:SS.mmm #<request> <what>`, the time in UTC. `>` is what came in, `<` what
//! went out, `sse` what the stream did.

use super::{bearer, Relay, BODY_LIMIT};
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Where the lines go: stdout when serving, a collector in tests.
pub type Sink = Arc<dyn Fn(String) + Send + Sync>;

/// Bodies beyond this are cut, with a count of what was left out. A full pull page or push batch
/// can run to megabytes; the start of it is what a reader looks at.
const BODY_CAP: usize = 16 * 1024;

static NEXT: AtomicU64 = AtomicU64::new(1);

/// The request number, handed to the event stream so its lines carry the same one.
#[derive(Clone, Copy)]
pub(crate) struct Traced(pub u64);

pub fn stdout() -> Sink {
    Arc::new(|line| println!("{line}"))
}

fn emit(sink: &Sink, id: u64, what: &str) {
    sink(format!("{} #{id} {what}", clock()));
}

pub(crate) async fn trace(State(r): State<Relay>, req: Request, next: Next) -> Response {
    let Some(sink) = r.trace.clone() else { return next.run(req).await };
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let (mut parts, body) = req.into_parts();
    let head = format!("> {} {} {}", parts.method, parts.uri, who(&r, &parts.headers));
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        emit(&sink, id, &format!("{head} body over {BODY_LIMIT} bytes or cut off"));
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    emit(&sink, id, &format!("{head}{}", shown(&bytes)));

    parts.extensions.insert(Traced(id));
    let res = next.run(Request::from_parts(parts, Body::from(bytes))).await;
    let ms = started.elapsed().as_millis();
    let streaming = res.headers().get(header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    if streaming {
        emit(&sink, id, &format!("< {} {ms}ms event stream", res.status()));
        return res;
    }
    let (parts, body) = res.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
    emit(&sink, id, &format!("< {} {ms}ms{}", parts.status, shown(&bytes)));
    Response::from_parts(parts, Body::from(bytes))
}

/// Who a request is from, without the key.
fn who(r: &Relay, headers: &HeaderMap) -> String {
    match bearer(headers) {
        None => "(no key)".into(),
        // `lookup`, not `authenticate`: watching must not give an unnamed key its name.
        Some(key) => match r.db().lookup(key) {
            Ok((_, Some((tag, name)))) => format!("{name}/{tag}"),
            Ok((id, None)) => format!("(key {id}, not named yet)"),
            Err(_) => {
                let key = key.trim();
                format!("(unknown key ...{})", &key[key.len().saturating_sub(4)..])
            }
        },
    }
}

fn shown(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let sent = bytes.len();
    let bytes = &*one_line(bytes);
    let cut = bytes.len().min(BODY_CAP);
    let more = match bytes.len() - cut {
        0 => String::new(),
        n => format!(" ... {n} bytes more"),
    };
    format!(" {sent}B {}{more}", String::from_utf8_lossy(&bytes[..cut]))
}

/// JSON re-written on one line, so every event stays one line however the client formatted
/// it; the byte count `shown` prints is still what was sent. Anything else as it came.
fn one_line(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if bytes.contains(&b'\n') {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
            return serde_json::to_vec(&v).map(Into::into).unwrap_or(bytes.into());
        }
    }
    bytes.into()
}

fn clock() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let s = now.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, s / 60 % 60, s % 60, now.subsec_millis())
}

/// What one event stream does, from opening to closing. Its drop is the closing line, so a
/// client that went away is logged as well as a stream the relay ended.
pub(crate) struct StreamLog {
    sink: Sink,
    id: u64,
    opened: Instant,
    pub why: &'static str,
}

impl StreamLog {
    pub fn new(sink: Sink, id: u64, who: &str, room: &str) -> Self {
        emit(&sink, id, &format!("sse open {who} room {room}"));
        Self { sink, id, opened: Instant::now(), why: "client went away" }
    }

    pub fn head(&self, head: i64) {
        emit(&self.sink, self.id, &format!("sse head {head}"));
    }
}

impl Drop for StreamLog {
    fn drop(&mut self) {
        emit(&self.sink, self.id, &format!("sse closed after {}s: {}", self.opened.elapsed().as_secs(), self.why));
    }
}
