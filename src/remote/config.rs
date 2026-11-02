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
    crate::core::sync::sync_enabled()
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
        Self::load_file()
    }

    /// The remote this machine has saved, ignoring any override in the environment.
    pub fn load_file() -> crate::core::Result<Option<Self>> {
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

/// What became of the relay named in the plugin's settings (`userConfig` in the manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adoption {
    /// The settings name no relay: the plugin stays local, as most installs do.
    Unset,
    /// This machine had no remote, and now has the one from the settings.
    Adopted(RemoteConfig),
    /// This machine already uses exactly that relay and key.
    Same,
    /// This machine already uses another relay or key, with boards connected through it, and
    /// keeps it: nothing local can tell a rotated key from a stale one in another Claude
    /// configuration, and taking the wrong one would cut every connected board off.
    Kept { machine: RemoteConfig },
}

/// Claude Code hands the settings to the MCP server through these. A setting never filled in can
/// arrive empty, or as the unsubstituted `${user_config.…}` reference.
const PLUGIN_URL: &str = "AI_KANBAN_PLUGIN_RELAY_URL";
const PLUGIN_KEY: &str = "AI_KANBAN_PLUGIN_RELAY_KEY";
const PLUGIN_NAME: &str = "AI_KANBAN_PLUGIN_RELAY_NAME";

/// The name from the plugin's settings, if one is filled in. Sent on every start, so the
/// settings are where a plugin user's name lives: a rename with `ai-kanban remote --name` holds
/// only while the setting is empty.
pub fn wanted_name() -> Option<String> {
    setting(std::env::var(PLUGIN_NAME).ok())
}

fn setting(raw: Option<String>) -> Option<String> {
    raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty() && !v.starts_with("${"))
}

/// Saves the relay from the plugin's settings as this machine's remote (see `decide`). Purely
/// local: the first sync round asks the relay who the key belongs to.
pub fn adopt_plugin_settings(connected: bool) -> crate::core::Result<Adoption> {
    let (url, key) = (setting(std::env::var(PLUGIN_URL).ok()), setting(std::env::var(PLUGIN_KEY).ok()));
    let decision = decide(url.zip(key), RemoteConfig::load_file()?, connected);
    if let Adoption::Adopted(cfg) = &decision {
        cfg.save()?;
    }
    Ok(decision)
}

/// The rule, apart from the environment and the file: with no board connected the settings
/// win; with boards connected the machine's remote does.
///
/// With nothing connected there is nothing to protect, and the settings menu is where people
/// put a key: a relay wiped and set up again, or a new key, must take effect without a
/// terminal. It used to lose to any remote already saved, leaving a dead key in use and a
/// heads-up at every start on a machine with nothing shared.
///
/// With boards connected the machine keeps its remote, because nothing local can tell which of
/// two keys is the good one: a rotated key in the menu looks the same as a stale one in a
/// second Claude configuration (`~/.claude`, `~/.claude-team`) sharing this store and its one
/// `remote.json` -- and taking that one at every start of that configuration would cut the
/// connected boards off for both. Only the relay can tell; until it is asked, the agent gets a
/// heads-up.
pub fn decide(settings: Option<(String, String)>, machine: Option<RemoteConfig>, connected: bool) -> Adoption {
    let Some((url, key)) = settings else { return Adoption::Unset };
    let offered = RemoteConfig { url: normalize_url(&url), key };
    match machine {
        Some(m) if m == offered => Adoption::Same,
        Some(m) if connected => Adoption::Kept { machine: m },
        _ => Adoption::Adopted(offered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str, key: &str) -> RemoteConfig {
        RemoteConfig { url: url.into(), key: key.into() }
    }

    #[test]
    fn an_unfilled_setting_is_no_setting() {
        assert_eq!(setting(None), None);
        assert_eq!(setting(Some("  ".into())), None);
        assert_eq!(setting(Some("${user_config.relay_key}".into())), None);
        assert_eq!(setting(Some(" akb_1 ".into())).as_deref(), Some("akb_1"));
    }

    #[test]
    fn the_settings_win_unless_boards_are_connected() {
        let s = || Some(("kanban.example.com".to_string(), "akb_1".to_string()));
        let offered = cfg("https://kanban.example.com", "akb_1");
        for connected in [false, true] {
            assert_eq!(decide(None, Some(cfg("https://a", "k")), connected), Adoption::Unset);
            assert_eq!(decide(s(), None, connected), Adoption::Adopted(offered.clone()));
            assert_eq!(decide(s(), Some(offered.clone()), connected), Adoption::Same);
        }
        // Nothing connected, nothing to protect: the relay set up again, a new key in the menu.
        for machine in [cfg("https://kanban.example.com", "akb_dead"), cfg("https://other", "akb_1")] {
            assert_eq!(decide(s(), Some(machine), false), Adoption::Adopted(offered.clone()));
        }
        // Connected: a stale key in a second Claude configuration must not cut the boards off.
        for machine in [cfg("https://kanban.example.com", "akb_2"), cfg("https://other", "akb_1")] {
            assert_eq!(decide(s(), Some(machine.clone()), true), Adoption::Kept { machine });
        }
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
