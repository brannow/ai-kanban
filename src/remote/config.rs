//! Which relay this machine talks to, and with which key. One remote per client (docs/sync.md).
//!
//! Stored in `remote.json` next to the store, so `AI_KANBAN_DB` isolates it together with the
//! store in tests and dev runs. Never in SQLite: `backup` and `export` would carry the key.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    pub key: String,
}

/// Whether this run may use the network at all. `AI_KANBAN_SYNC=off` keeps one Claude run (or
/// shell) local-only without changing anything stored: connected boards keep recording, and
/// sync picks up where it left off in the next run that allows it.
pub fn sync_enabled() -> bool {
    !std::env::var("AI_KANBAN_SYNC").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

impl RemoteConfig {
    pub fn path() -> crate::core::Result<PathBuf> {
        let store = crate::core::Store::default_path()?;
        Ok(store.parent().map(|d| d.join("remote.json")).unwrap_or_else(|| PathBuf::from("remote.json")))
    }

    /// The remote, if one is set. `AI_KANBAN_SERVER_URL` with `AI_KANBAN_SERVER_KEY` stands in
    /// for the file, for CI and throwaway setups.
    pub fn load() -> crate::core::Result<Option<Self>> {
        if let (Ok(url), Ok(key)) = (std::env::var("AI_KANBAN_SERVER_URL"), std::env::var("AI_KANBAN_SERVER_KEY")) {
            return Ok(Some(RemoteConfig { url: normalize_url(&url), key: key.trim().into() }));
        }
        let path = Self::path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| crate::core::Error::Other(format!("{} is not a valid remote config: {e}", path.display())))
    }

    /// Writes the file readable by its owner only -- created that way, not chmod-ed after, so
    /// the key is never readable by others even for a moment.
    pub fn save(&self) -> crate::core::Result<()> {
        use std::io::Write;
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = open_private(&tmp)?;
        f.write_all(serde_json::to_string_pretty(self).unwrap_or_default().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn remove() -> crate::core::Result<()> {
        match std::fs::remove_file(Self::path()?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The key with all but its first characters hidden, for printing.
    pub fn masked_key(&self) -> String {
        let shown: String = self.key.chars().take(8).collect();
        format!("{shown}…")
    }
}

#[cfg(unix)]
fn open_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

#[cfg(not(unix))]
fn open_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

/// One spelling per relay: no trailing slash, `https://` assumed when no scheme is given.
pub fn normalize_url(raw: &str) -> String {
    let u = raw.trim().trim_end_matches('/');
    if u.contains("://") { u.to_string() } else { format!("https://{u}") }
}
