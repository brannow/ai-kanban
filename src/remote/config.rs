//! Which relay this machine talks to, and with which key. One remote per client (docs/sync.md).
//!
//! Stored in `remote.json` next to the store, so `AI_KANBAN_DB` isolates it together with the
//! store in tests and dev runs. Never in SQLite: `backup` and `export` would carry the key.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    pub key: String,
    /// Fingerprints (`fingerprint`) of the relays and keys this machine moved away from. Plugin
    /// settings naming one of them are a second Claude configuration not updated yet, never a
    /// move: taking them would send the machine back and forth (see `decide`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left: Vec<String>,
    /// Per Claude configuration (by its plugin data directory), the fingerprint of the relay and
    /// key its settings named when it last started. A configuration switches the machine only
    /// when its own settings changed since: one that merely disagrees with a switch made
    /// elsewhere -- in the other configuration, or with `ai-kanban remote` -- is behind, not new.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub seen: std::collections::BTreeMap<String, String>,
}

/// Whether this run may use the network at all. `AI_KANBAN_SYNC=off` keeps one Claude run (or
/// shell) local-only without changing anything stored: connected boards keep recording, and
/// sync picks up where it left off in the next run that allows it.
pub fn sync_enabled() -> bool {
    crate::core::sync::sync_enabled()
}

impl RemoteConfig {
    pub fn new(url: &str, key: &str) -> Self {
        RemoteConfig { url: normalize_url(url), key: key.trim().into(), left: vec![], seen: Default::default() }
    }

    /// Whether both name the same relay with the same key, whatever they remember.
    pub fn same_remote(&self, other: &RemoteConfig) -> bool {
        self.url == other.url && self.key == other.key
    }

    /// `next`, remembering that this machine moved away from `self` (and from what `self`
    /// already remembered).
    pub fn replaced_by(&self, next: &RemoteConfig) -> RemoteConfig {
        let mut left = self.left.clone();
        left.push(fingerprint(self));
        left.retain(|f| *f != fingerprint(next));
        left.sort();
        left.dedup();
        RemoteConfig { url: next.url.clone(), key: next.key.clone(), left, seen: self.seen.clone() }
    }

    pub fn path() -> crate::core::Result<PathBuf> {
        let store = crate::core::Store::default_path()?;
        Ok(store.parent().map(|d| d.join("remote.json")).unwrap_or_else(|| PathBuf::from("remote.json")))
    }

    /// The remote, if one is set. `AI_KANBAN_SERVER_URL` with `AI_KANBAN_SERVER_KEY` stands in
    /// for the file, for CI and throwaway setups.
    pub fn load() -> crate::core::Result<Option<Self>> {
        if let (Ok(url), Ok(key)) = (std::env::var("AI_KANBAN_SERVER_URL"), std::env::var("AI_KANBAN_SERVER_KEY")) {
            return Ok(Some(RemoteConfig::new(&url, &key)));
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
    /// The settings name another relay or key than this machine had, and it takes them: a move
    /// to another relay, a new key, or a first remote. `replaced` is what it used before, which
    /// is drained before anything reaches the new one (docs/sync.md, "Moving to another relay").
    Adopted { cfg: RemoteConfig, replaced: Option<RemoteConfig> },
    /// This machine already uses exactly that relay and key.
    Same,
    /// The settings name another relay or key than the machine uses, and this configuration's
    /// settings did not change since it last started: the switch was made elsewhere (the other
    /// Claude configuration, `ai-kanban remote`), and this one is behind. Kept what the machine
    /// uses now. `left`: the settings name one the machine already moved away from.
    Kept { machine: RemoteConfig, left: bool },
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
/// local: the first sync round asks the relay who the key belongs to. `connected`: whether any
/// board is shared, which decides what a configuration seen for the first time may do.
pub fn adopt_plugin_settings(connected: bool) -> crate::core::Result<Adoption> {
    let (url, key) = (setting(std::env::var(PLUGIN_URL).ok()), setting(std::env::var(PLUGIN_KEY).ok()));
    // Each Claude configuration has its own plugin data directory; outside the plugin there is
    // none, and a run is treated as a configuration seen for the first time.
    let config = std::env::var("CLAUDE_PLUGIN_DATA").ok().filter(|d| !d.trim().is_empty());
    let (decision, save) = decide(url.zip(key), RemoteConfig::load_file()?, config.as_deref(), connected);
    if let Some(cfg) = save {
        cfg.save()?;
    }
    Ok(decision)
}

/// The rule, apart from the environment and the file. Returns what to do, and what to save.
///
/// New settings win: the settings menu is where people put a relay and a key, and where a
/// team's move to another relay is made -- each member enters the new URL and key, and nothing
/// else (docs/sync.md, "Moving to another relay").
///
/// "New" is this configuration's settings having changed since it last started, not their
/// differing from the machine's remote. Two Claude configurations (`~/.claude`, `~/.claude-team`)
/// share this store and its one `remote.json`, and one of them can be behind: switched away from
/// in the other, or by `ai-kanban remote`. Taking its settings at every start of it flipped the
/// boards back and forth (0.2.7), which is why the machine's remote used to win outright once a
/// board was connected. So:
/// - settings naming a relay and key the machine moved away from are behind, always;
/// - settings unchanged since this configuration last started are behind too;
/// - a configuration not seen before (the first start after an upgrade to 0.3.0 included) does
///   what it did before: switches the machine only if no board is connected, and from then on
///   is judged by whether its settings change.
pub fn decide(settings: Option<(String, String)>, machine: Option<RemoteConfig>, config: Option<&str>, connected: bool) -> (Adoption, Option<RemoteConfig>) {
    let Some((url, key)) = settings else { return (Adoption::Unset, None) };
    let mut offered = RemoteConfig::new(&url, &key);
    let now = fingerprint(&offered);
    let config = config.map(|c| fingerprint(&RemoteConfig::new(c, "")));
    let Some(m) = machine else {
        if let Some(c) = config {
            offered.seen.insert(c, now);
        }
        return (Adoption::Adopted { cfg: offered.clone(), replaced: None }, Some(offered));
    };
    let last = config.as_ref().and_then(|c| m.seen.get(c)).cloned();
    let mut recorded = m.clone();
    if let Some(c) = &config {
        recorded.seen.insert(c.clone(), now.clone());
    }
    let save = (recorded != m).then(|| recorded.clone());
    if m.same_remote(&offered) {
        return (Adoption::Same, save);
    }
    if m.left.contains(&now) {
        return (Adoption::Kept { machine: m, left: true }, save);
    }
    let changed = match &last {
        Some(before) => *before != now,
        None => !connected,
    };
    if !changed {
        return (Adoption::Kept { machine: m, left: false }, save);
    }
    let cfg = recorded.replaced_by(&offered);
    (Adoption::Adopted { cfg: cfg.clone(), replaced: Some(m) }, Some(cfg))
}

/// A relay and key, recognizable without being kept: `remote.json` remembers what this machine
/// left by this, not by the old key. FNV-1a, because recognizing a pair is all it is for --
/// not a secret's protection (the file is the owner's alone anyway) -- and it needs no crate.
pub fn fingerprint(c: &RemoteConfig) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in c.url.bytes().chain(std::iter::once(b'\n')).chain(c.key.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str, key: &str) -> RemoteConfig {
        RemoteConfig::new(url, key)
    }

    #[test]
    fn an_unfilled_setting_is_no_setting() {
        assert_eq!(setting(None), None);
        assert_eq!(setting(Some("  ".into())), None);
        assert_eq!(setting(Some("${user_config.relay_key}".into())), None);
        assert_eq!(setting(Some(" akb_1 ".into())).as_deref(), Some("akb_1"));
    }

    #[test]
    fn a_configuration_switches_the_machine_only_when_its_own_settings_change() {
        let s = |u: &str, k: &str| Some((u.to_string(), k.to_string()));
        let (team, home) = (Some("/u/.claude-team/plugins/data/ai-kanban"), Some("/u/.claude/plugins/data/ai-kanban"));
        let a = cfg("https://a.example.com", "akb_1");
        assert_eq!(decide(None, Some(a.clone()), team, true).0, Adoption::Unset);
        let (first, saved) = decide(s("a.example.com", "akb_1"), None, team, false);
        assert!(matches!(first, Adoption::Adopted { replaced: None, .. }));
        let a = saved.unwrap();
        // Both configurations start with the same settings: recorded, nothing switches.
        let (same, saved) = decide(s("a.example.com", "akb_1"), Some(a.clone()), home, true);
        assert_eq!(same, Adoption::Same);
        let a = saved.unwrap();

        // The team moves: new settings in one configuration. Taken, the old pair remembered.
        let (moved, saved) = decide(s("b.example.com", "akb_2"), Some(a.clone()), team, true);
        let Adoption::Adopted { cfg: b, replaced } = moved else { panic!("{moved:?}") };
        assert_eq!((b.url.as_str(), replaced.map(|r| r.url)), ("https://b.example.com", Some("https://a.example.com".into())));
        assert_eq!(saved.as_ref(), Some(&b));
        assert!(!serde_json::to_string(&b).unwrap().contains("akb_1"), "the old key itself is not kept");
        // The other configuration, not updated: behind, every time.
        let (behind, _) = decide(s("a.example.com", "akb_1"), Some(b.clone()), home, true);
        assert_eq!(behind, Adoption::Kept { machine: b.clone(), left: true });
        // A new key on the new relay in it (rotated): its settings changed, taken.
        assert!(matches!(decide(s("b.example.com", "akb_3"), Some(b.clone()), home, true).0, Adoption::Adopted { .. }));
    }

    /// Upgrading to 0.3.0 with two configurations that disagree: the one the machine was not
    /// following kept getting "Kept" before, and has no record yet. It must not switch the
    /// machine at its first start -- only once its settings change.
    #[test]
    fn after_an_upgrade_a_configuration_that_was_kept_is_still_kept() {
        let s = |u: &str, k: &str| Some((u.to_string(), k.to_string()));
        let machine = cfg("https://good.example.com", "akb_good");
        let stale = || s("old.example.com", "akb_stale");
        let (first, saved) = decide(stale(), Some(machine.clone()), Some("/u/.claude-team"), true);
        assert_eq!(first, Adoption::Kept { machine: machine.clone(), left: false });
        let recorded = saved.expect("what it saw is recorded");
        assert_eq!(decide(stale(), Some(recorded.clone()), Some("/u/.claude-team"), true).0, Adoption::Kept { machine: recorded.clone(), left: false });
        // Its settings updated later: now it switches.
        assert!(matches!(decide(s("new.example.com", "akb_new"), Some(recorded), Some("/u/.claude-team"), true).0, Adoption::Adopted { .. }));
        // With nothing connected there is nothing to protect, as before.
        assert!(matches!(decide(stale(), Some(machine), Some("/u/.claude-team"), false).0, Adoption::Adopted { .. }));
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
