//! Putting `ai-kanban` on the user's own PATH when the plugin came from the marketplace.
//!
//! The plugin's binary lives in a directory named after its version, so a link straight to it
//! breaks on the next update. Two links instead:
//!
//!   ~/.local/bin/ai-kanban -> <plugin data>/bin/ai-kanban -> <current version>/libexec/ai-kanban-…
//!
//! The plugin's data directory survives updates. The MCP server re-points the inner link at its
//! own binary on every start, so after an update the next session moves it along. The outer
//! link is made once, by `install-cli`, when the user asks for it -- nothing here writes into a
//! user's PATH unasked. The inner link names the binary itself, not `bin/ai-kanban`: that
//! wrapper finds `libexec/` from its own location, which a link would move.

use std::path::{Path, PathBuf};

/// Points `<data>/bin/ai-kanban` at `exe`, replacing whatever it pointed at. Run by the MCP
/// server at startup; a failure costs only the terminal command, never the session.
pub fn refresh_stable_link(data: &Path, exe: &Path) -> std::io::Result<PathBuf> {
    let dir = data.join("bin");
    std::fs::create_dir_all(&dir)?;
    let link = dir.join("ai-kanban");
    if std::fs::read_link(&link).ok().as_deref() == Some(exe) {
        return Ok(link);
    }
    // Made aside and renamed over: a terminal command running at this moment sees the old
    // link or the new one, never none.
    let tmp = dir.join(format!(".ai-kanban.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(exe, &tmp)?;
    std::fs::rename(&tmp, &link)?;
    Ok(link)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Created,
    /// The link was already there and pointed where it should.
    Already,
    /// Something else was there, and `--replace` took its place.
    Replaced,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The plugin has not set up its link yet: no session has started since it was installed.
    NoStableLink(PathBuf),
    /// Something other than our link is there. Said, not overwritten: it may be a binary the
    /// user built or copied there on purpose.
    Occupied { at: PathBuf, what: String },
}

/// Links `<dir>/ai-kanban` to the plugin's stable link.
pub fn install(dir: &Path, stable: &Path, replace: bool) -> Result<Installed, Refused> {
    if std::fs::symlink_metadata(stable).is_err() {
        return Err(Refused::NoStableLink(stable.to_path_buf()));
    }
    let at = dir.join("ai-kanban");
    let mut outcome = Installed::Created;
    if let Ok(meta) = std::fs::symlink_metadata(&at) {
        let target = std::fs::read_link(&at).ok();
        if target.as_deref() == Some(stable) {
            return Ok(Installed::Already);
        }
        if !replace {
            let what = match target {
                Some(t) => format!("a link to {}", t.display()),
                None if meta.is_dir() => "a directory".to_string(),
                None => format!("a file of {} bytes", meta.len()),
            };
            return Err(Refused::Occupied { at, what });
        }
        if meta.is_dir() {
            return Err(Refused::Occupied { at, what: "a directory, which --replace does not remove".into() });
        }
        std::fs::remove_file(&at).map_err(|e| Refused::Occupied { at: at.clone(), what: format!("not removable: {e}") })?;
        outcome = Installed::Replaced;
    }
    std::fs::create_dir_all(dir).map_err(|e| Refused::Occupied { at: dir.to_path_buf(), what: format!("not creatable: {e}") })?;
    std::os::unix::fs::symlink(stable, &at).map_err(|e| Refused::Occupied { at: at.clone(), what: format!("not writable: {e}") })?;
    Ok(outcome)
}

/// Whether `dir` is on `path` (a `PATH` value), so the command will be found by name.
pub fn on_path(dir: &Path, path: &str) -> bool {
    std::env::split_paths(path).any(|p| p == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stable_link_follows_the_running_binary_across_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let (v1, v2) = (tmp.path().join("0.1.0/ai-kanban"), tmp.path().join("0.2.0/ai-kanban"));
        let link = refresh_stable_link(tmp.path(), &v1).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), v1);
        refresh_stable_link(tmp.path(), &v1).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), v1, "unchanged when nothing changed");
        refresh_stable_link(tmp.path(), &v2).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), v2, "an update moves it along");
        let left: Vec<_> = std::fs::read_dir(tmp.path().join("bin")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["ai-kanban"], "no temporary link left behind");
    }

    #[test]
    fn installing_links_once_and_never_overwrites_what_it_did_not_make() {
        let tmp = tempfile::tempdir().unwrap();
        let stable = refresh_stable_link(&tmp.path().join("data"), &tmp.path().join("bin-v1")).unwrap();
        let dir = tmp.path().join("local/bin");

        assert_eq!(install(&dir, &stable, false), Ok(Installed::Created), "creates the directory too");
        assert_eq!(std::fs::read_link(dir.join("ai-kanban")).unwrap(), stable);
        assert_eq!(install(&dir, &stable, false), Ok(Installed::Already));

        std::fs::remove_file(dir.join("ai-kanban")).unwrap();
        std::fs::write(dir.join("ai-kanban"), b"a binary someone copied here").unwrap();
        match install(&dir, &stable, false) {
            Err(Refused::Occupied { what, .. }) => assert!(what.contains("file of 28 bytes"), "{what}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(dir.join("ai-kanban")).unwrap(), b"a binary someone copied here", "left alone");
        assert_eq!(install(&dir, &stable, true), Ok(Installed::Replaced));
        assert_eq!(std::fs::read_link(dir.join("ai-kanban")).unwrap(), stable);
    }

    #[test]
    fn without_a_session_since_install_there_is_nothing_to_link_to() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("data/bin/ai-kanban");
        assert_eq!(install(&tmp.path().join("bin"), &missing, false), Err(Refused::NoStableLink(missing)));
        assert!(!tmp.path().join("bin").exists(), "nothing created");
    }

    #[test]
    fn path_membership_is_by_whole_entry() {
        let dir = Path::new("/Users/x/.local/bin");
        assert!(on_path(dir, "/usr/bin:/Users/x/.local/bin:/bin"));
        assert!(!on_path(dir, "/usr/bin:/Users/x/.local/bin2"));
    }
}
