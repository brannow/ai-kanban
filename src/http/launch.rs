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

/// Opens a new Ghostty window in `cwd` running `claude <prompt>`, with each of `add_dirs`
/// passed as `--add-dir` so a ticket spanning several repos can reach all of them.
pub fn open_claude(profile: Profile, cwd: &str, add_dirs: &[String], prompt: &str) -> Result<(), Error> {
    if !Path::new(GHOSTTY).exists() {
        return Err(Error::Other(format!("Ghostty is not installed at {GHOSTTY}")));
    }
    let claude = claude().ok_or_else(|| Error::Other("could not find the `claude` command".into()))?;
    let mut cmd = Command::new("open");
    // `-n`: a new instance, so the session gets its own window even when Ghostty is open.
    cmd.args(["-na", GHOSTTY, "--args"])
        .arg(format!("--working-directory={cwd}"))
        .arg("-e");
    if profile == Profile::ClaudeWork {
        let dir = dirs::home_dir()
            .ok_or_else(|| Error::Other("no home directory to find ~/.claude-work in".into()))?
            .join(".claude-work");
        // `env` rather than a shell: it sets the variable and execs, and parses nothing else.
        cmd.arg("/usr/bin/env").arg(format!("CLAUDE_CONFIG_DIR={}", dir.display()));
    }
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
