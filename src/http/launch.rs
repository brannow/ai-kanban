//! Starting work from the board: a new Ghostty window running Claude Code in a ticket's repo.
//!
//! This is the one place `serve` starts a process, which makes it the one place a web page
//! could try to make it start one. The route checks the request came from the board page
//! itself (`routes::start_task`), and nothing here goes through a shell: the prompt is a
//! single argv entry, so no text in a ticket -- which an agent may have written -- can become
//! a command.

use crate::core::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

const GHOSTTY: &str = "/Applications/Ghostty.app";

/// Which Claude Code setup a session starts in.
///
/// `Work` is the user's `claude-work` shell alias: the same binary with
/// `CLAUDE_CONFIG_DIR=~/.claude-work` -- its own login, settings and plugins. The alias
/// itself cannot be used: a window started through `open` runs no shell, so it never sees
/// aliases. The environment variable is set directly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Claude,
    ClaudeWork,
}

impl Profile {
    pub fn parse(raw: Option<&str>) -> Result<Self, Error> {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("claude") => Ok(Profile::Claude),
            Some("claude-work") => Ok(Profile::ClaudeWork),
            Some(other) => Err(Error::InvalidValue {
                field: "profile",
                value: other.to_string(),
                valid: "claude, claude-work".into(),
            }),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Profile::Claude => "claude",
            Profile::ClaudeWork => "claude-work",
        }
    }
}

/// `claude` by absolute path. A window started through `open` gets launchd's environment, not
/// the user's shell, so a bare `claude` would not be on its PATH.
fn claude() -> Option<PathBuf> {
    let on_path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("claude")).collect())
        .unwrap_or_default();
    let installer = dirs::home_dir().map(|h| h.join(".local/bin/claude"));
    on_path.into_iter().chain(installer).find(|p| p.is_file())
}

/// Claude Code's own environment, which `serve` inherits when it was started from inside a
/// session: `CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, the messaging socket,
/// `CLAUDE_CODE_CHILD_SESSION`. A session started with them believes it is a child of that
/// one -- it turns transcript saving off and talks to the parent's socket, which disrupts
/// sessions already running. A parent's `CLAUDE_CONFIG_DIR` goes too, or "open in claude"
/// from a serve started under claude-work would land in the work setup.
fn is_session_var(name: &str) -> bool {
    name.starts_with("CLAUDE")
}

/// The `/usr/bin/env` argv that starts `claude` as its own top-level session: every inherited
/// session variable unset, and the profile's config dir set. `env` rather than a shell: it
/// sets and unsets, then execs, and parses nothing else.
fn env_prefix(inherited: &[String], config_dir: Option<&Path>) -> Vec<String> {
    let mut args = vec!["/usr/bin/env".to_string()];
    for k in inherited {
        args.push("-u".into());
        args.push(k.clone());
    }
    if let Some(dir) = config_dir {
        args.push(format!("CLAUDE_CONFIG_DIR={}", dir.display()));
    }
    args
}

/// Opens a new Ghostty window in `cwd` running `claude <prompt>`, with each of `add_dirs`
/// passed as `--add-dir` so a ticket spanning several repos can reach all of them.
pub fn open_claude(profile: Profile, cwd: &str, add_dirs: &[String], prompt: &str) -> Result<(), Error> {
    if !Path::new(GHOSTTY).exists() {
        return Err(Error::Other(format!("Ghostty is not installed at {GHOSTTY}")));
    }
    let claude = claude().ok_or_else(|| Error::Other("could not find the `claude` command".into()))?;
    let config_dir = match profile {
        Profile::Claude => None,
        Profile::ClaudeWork => Some(
            dirs::home_dir()
                .ok_or_else(|| Error::Other("no home directory to find ~/.claude-work in".into()))?
                .join(".claude-work"),
        ),
    };
    let inherited: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| is_session_var(k))
        .collect();
    let mut cmd = Command::new("open");
    // Also keep them off `open` itself, in case Launch Services passes its environment on.
    for k in &inherited {
        cmd.env_remove(k);
    }
    // `-n`: a new instance, so the session gets its own window even when Ghostty is open.
    cmd.args(["-na", GHOSTTY, "--args"])
        .arg(format!("--working-directory={cwd}"))
        .arg("-e");
    cmd.args(env_prefix(&inherited, config_dir.as_deref()));
    cmd.arg(&claude);
    for d in add_dirs {
        cmd.arg("--add-dir").arg(d);
    }
    cmd.arg(prompt);
    // `open` hands off to Launch Services and returns at once; this does not wait for the session.
    let status = cmd.status()?;
    if !status.success() {
        return Err(Error::Other(format!("`open` could not start Ghostty ({status})")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_every_claude_var_but_nothing_else() {
        for k in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_PID", "CLAUDE_CONFIG_DIR"] {
            assert!(is_session_var(k), "{k}");
        }
        for k in ["PATH", "HOME", "ANTHROPIC_API_KEY", "MY_CLAUDE"] {
            assert!(!is_session_var(k), "{k}");
        }
    }

    #[test]
    fn unsets_inherited_vars_before_setting_the_profile_dir() {
        let inherited = vec!["CLAUDE_CODE_CHILD_SESSION".to_string(), "CLAUDE_CONFIG_DIR".to_string()];
        assert_eq!(
            env_prefix(&inherited, Some(Path::new("/h/.claude-work"))),
            ["/usr/bin/env", "-u", "CLAUDE_CODE_CHILD_SESSION", "-u", "CLAUDE_CONFIG_DIR", "CLAUDE_CONFIG_DIR=/h/.claude-work"]
        );
        assert_eq!(env_prefix(&inherited, None), ["/usr/bin/env", "-u", "CLAUDE_CODE_CHILD_SESSION", "-u", "CLAUDE_CONFIG_DIR"]);
    }
}
