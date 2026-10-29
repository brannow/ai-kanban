//! The relay's API, as blocking calls with hard timeouts.
//!
//! Blocking because every caller is: the CLI, the MCP tool handlers (synchronous functions) and
//! the sync thread. `ureq` has no overall timeout by default; one here is what keeps a tool call
//! that pulls before a claim from hanging the session when the relay is unreachable.

use crate::sync::protocol::{Change, Hello, Joined, Page, Push, Pushed, RoomInfo, WhoAmI};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The relay could not be reached: no network, DNS, refused, timed out.
    #[error("relay unreachable: {0}")]
    Offline(String),
    #[error("the relay rejected the key (revoked, or for another relay)")]
    Unauthorized,
    #[error("{0}")]
    NotFound(String),
    /// The relay holds something that contradicts this change -- a reused change id.
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("relay error {0}: {1}")]
    Http(u16, String),
    /// Sync is stopped for this store until a person acts; the message says what to do.
    #[error("sync paused: {0}")]
    Paused(String),
    /// The relay needs a newer ai-kanban than this one (426).
    #[error("{0}")]
    Upgrade(String),
}

impl RemoteError {
    /// What kind of failure this is, in a word or two, for the board header: "offline" says
    /// "it will pass", everything else says "someone has to act".
    pub fn kind(&self) -> &'static str {
        match self {
            RemoteError::Offline(_) => "offline",
            RemoteError::Unauthorized => "key rejected",
            RemoteError::NotFound(_) => "room not found",
            RemoteError::Conflict(_) => "conflict",
            RemoteError::BadRequest(_) | RemoteError::Http(..) => "relay error",
            RemoteError::Paused(_) => "paused",
            RemoteError::Upgrade(_) => "update needed",
        }
    }

    /// How it is recorded as a board's last error: its kind, then the detail.
    pub fn recorded(&self) -> String {
        format!("{}: {self}", self.kind())
    }
}

impl From<RemoteError> for crate::core::Error {
    fn from(e: RemoteError) -> Self {
        crate::core::Error::Other(e.to_string())
    }
}

#[derive(Clone)]
pub struct Client {
    url: String,
    key: String,
    agent: ureq::Agent,
}

impl Client {
    /// `timeout` bounds each whole call, connecting included.
    pub fn new(url: &str, key: &str, timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .timeout_connect(Some(timeout.min(Duration::from_secs(5))))
            .http_status_as_error(false)
            // Which ai-kanban this is, for the relay's admin (`key list`) and for the relay to
            // refuse one too old for it.
            .user_agent(crate::sync::protocol::client_agent(env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Client { url: super::config::normalize_url(url), key: key.trim().to_string(), agent }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn room_url(&self, room: &str, rest: &str) -> String {
        format!("{}/v1/rooms/{}/{rest}", self.url, room)
    }

    pub fn hello(&self) -> Result<Hello, RemoteError> {
        let res = self.agent.get(format!("{}/v1/hello", self.url)).call().map_err(offline)?;
        read(res)
    }

    pub fn whoami(&self) -> Result<WhoAmI, RemoteError> {
        let res = self.agent.get(format!("{}/v1/whoami", self.url)).header("authorization", self.bearer()).call().map_err(offline)?;
        read(res)
    }

    /// Sets the name this machine goes by, and answers like `whoami`. The first call fixes the
    /// key's tag from the name, so it has to come before any other authenticated request: the
    /// relay gives a key used without a name a generated one.
    pub fn set_name(&self, name: &str) -> Result<WhoAmI, RemoteError> {
        let body = serde_json::to_vec(&crate::sync::protocol::SetName { name: name.to_string() }).map_err(|e| RemoteError::Http(0, e.to_string()))?;
        let res = self.agent
            .put(format!("{}/v1/me", self.url))
            .header("authorization", self.bearer())
            .header("content-type", "application/json")
            .send(body)
            .map_err(offline)?;
        read(res)
    }

    /// `set_name` when there is a name to set, else `whoami`. A relay from before names (no
    /// `/v1/me`) gets `whoami`, so a newer client still works with it.
    pub fn identify(&self, name: Option<&str>) -> Result<WhoAmI, RemoteError> {
        match name {
            Some(n) => match self.set_name(n) {
                Err(RemoteError::NotFound(_) | RemoteError::Http(405, _)) => self.whoami(),
                other => other,
            },
            None => self.whoami(),
        }
    }

    pub fn rooms(&self) -> Result<Vec<RoomInfo>, RemoteError> {
        let res = self.agent.get(format!("{}/v1/rooms", self.url)).header("authorization", self.bearer()).call().map_err(offline)?;
        read(res)
    }

    /// `board` names the joining board; the relay keeps it if this join creates the room.
    pub fn join(&self, room: &str, board: Option<&str>) -> Result<Joined, RemoteError> {
        let mut req = self.agent.post(self.room_url(room, "join")).header("authorization", self.bearer());
        if let Some(b) = board {
            req = req.query("board", b);
        }
        read(req.send_empty().map_err(offline)?)
    }

    pub fn leave(&self, room: &str) -> Result<(), RemoteError> {
        let res = self.agent.post(self.room_url(room, "leave")).header("authorization", self.bearer()).send_empty().map_err(offline)?;
        match res.status().as_u16() {
            204 | 200 => Ok(()),
            _ => read::<serde_json::Value>(res).map(|_| ()),
        }
    }

    pub fn push(&self, room: &str, changes: &[Change]) -> Result<Pushed, RemoteError> {
        let res = self.agent
            .post(self.room_url(room, "changes"))
            .header("authorization", self.bearer())
            .header("content-type", "application/json")
            // Not `send_json`: ureq pretty-prints it, which about doubles every push.
            .send(serde_json::to_vec(&Push { changes: changes.to_vec() }).map_err(|e| RemoteError::Http(0, e.to_string()))?)
            .map_err(offline)?;
        read(res)
    }

    pub fn pull(&self, room: &str, since: i64, limit: usize) -> Result<Page, RemoteError> {
        let res = self.agent
            .get(self.room_url(room, &format!("changes?since={since}&limit={limit}")))
            .header("authorization", self.bearer())
            .call()
            .map_err(offline)?;
        read(res)
    }

    /// Reads a room's notification stream, calling `on_line` with each head the relay announces
    /// and with `None` for anything else it sends (the heartbeat among it, so the caller can tell
    /// a live stream from a dead one), until the stream ends, goes quiet for longer than its
    /// heartbeat, or `on_line` returns false. Reconnecting is the caller's business.
    pub fn watch(&self, room: &str, mut on_line: impl FnMut(Option<i64>) -> bool) -> Result<(), RemoteError> {
        use std::io::BufRead;
        // No overall timeout -- the stream is meant to stay open -- but a read timeout above the
        // relay's 30s heartbeat, so a dead connection is noticed instead of waited on forever.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_recv_body(Some(Duration::from_secs(75)))
            .http_status_as_error(false)
            // Which ai-kanban this is, for the relay's admin (`key list`) and for the relay to
            // refuse one too old for it.
            .user_agent(crate::sync::protocol::client_agent(env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let res = agent.get(self.room_url(room, "events")).header("authorization", self.bearer()).call().map_err(offline)?;
        if res.status().as_u16() != 200 {
            return read::<serde_json::Value>(res).map(|_| ());
        }
        let reader = std::io::BufReader::new(res.into_body().into_reader());
        for line in reader.lines() {
            let line = line.map_err(|e| RemoteError::Offline(e.to_string()))?;
            let head = line.strip_prefix("data:").and_then(|d| d.trim().parse::<i64>().ok());
            if !on_line(head) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.key)
    }
}

fn offline(e: ureq::Error) -> RemoteError {
    RemoteError::Offline(e.to_string())
}

/// The relay answers errors as `{"error": {"code", "message"}}`; the message is written to be
/// shown, so it is passed on as it is.
fn read<T: serde::de::DeserializeOwned>(mut res: ureq::http::Response<ureq::Body>) -> Result<T, RemoteError> {
    let status = res.status().as_u16();
    if (200..300).contains(&status) {
        return res.body_mut().read_json::<T>().map_err(|e| RemoteError::Http(status, format!("unreadable answer: {e}")));
    }
    let text = res.body_mut().read_to_string().unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| text.trim().chars().take(200).collect());
    Err(match status {
        401 => RemoteError::Unauthorized,
        404 => RemoteError::NotFound(message),
        409 => RemoteError::Conflict(message),
        400 | 413 => RemoteError::BadRequest(message),
        426 => RemoteError::Upgrade(message),
        _ => RemoteError::Http(status, message),
    })
}
