//! Starting work from the board: Claude Code in a ticket's repo, in a new Ghostty tab when
//! Ghostty is open and in a new Ghostty window when it is not.
//!
//! This is the one place `serve` starts a process, which makes it the one place a web page
//! could try to make it start one. The route checks the request came from the board page
//! itself (`routes::start_task`), and no text from a ticket -- which an agent may have
//! written -- is ever parsed as a command. The window passes the prompt as one argv entry. The
//! tab has to hand Ghostty a command line, so that line is built from variable names alone and
//! every argument travels as a variable's value, which a shell expands but never parses.

use crate::core::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GHOSTTY: &str = "/Applications/Ghostty.app";

/// Which Claude Code setup a session starts in.
///
/// `Work` is the user's `claude-work` shell alias: the same binary with
/// `CLAUDE_CONFIG_DIR=~/.claude-work` -- its own login, settings and plugins. The alias
/// itself cannot be used: neither a tab nor a window started from here runs the user's shell,
/// so neither sees aliases. The environment variable is set directly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Claude,
    ClaudeWork,
}

impl Profile {
    pub const ALL: [Profile; 2] = [Profile::Claude, Profile::ClaudeWork];

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

/// Where a session opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    Tab,
    Window,
}

impl Opened {
    pub fn label(self) -> &'static str {
        match self {
            Opened::Tab => "tab",
            Opened::Window => "window",
        }
    }
}

/// `claude` by absolute path. Neither a tab nor a window started from here gets the user's
/// shell environment, so a bare `claude` would not be on its PATH.
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

/// Opens Ghostty in `cwd` running `claude <prompt>`, with each of `add_dirs` passed as
/// `--add-dir` so a ticket spanning several repos can reach all of them.
pub fn open_claude(profile: Profile, cwd: &str, add_dirs: &[String], prompt: &str) -> Result<Opened, Error> {
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
    let mut argv = env_prefix(&inherited, config_dir.as_deref());
    argv.push(claude.to_string_lossy().into_owned());
    for d in add_dirs {
        argv.push("--add-dir".into());
        argv.push(d.clone());
    }
    argv.push(prompt.to_string());

    // A tab first, so sessions gather in the window already open rather than each getting
    // its own. Whatever stops it -- Ghostty not running, AppleScript access refused, a Ghostty
    // too old to script -- falls back to a window, which needs none of that.
    if open_tab(cwd, &argv) {
        return Ok(Opened::Tab);
    }
    open_window(cwd, &argv, &inherited)?;
    Ok(Opened::Window)
}

/// Ghostty's AppleScript (1.3+): a tab in the front window, or a window when Ghostty runs with
/// none open. It refuses when Ghostty is not running, because telling it anything would launch
/// it, and it would open its own default window next to ours.
///
/// Everything arrives as `argv`, so nothing is spliced into the script's source.
const TAB_SCRIPT: &str = r#"on run argv
	if application id "com.mitchellh.ghostty" is not running then error "Ghostty is not running"
	tell application id "com.mitchellh.ghostty"
		set cfg to new surface configuration
		set initial working directory of cfg to item 1 of argv
		set command of cfg to item 2 of argv
		set environment variables of cfg to items 3 thru -1 of argv
		if (count of windows) > 0 then
			new tab in front window with configuration cfg
		else
			new window with configuration cfg
		end if
		activate
	end tell
end run"#;

const ARG_VAR: &str = "AI_KANBAN_ARG_";

fn open_tab(cwd: &str, argv: &[String]) -> bool {
    let mut cmd = Command::new("/usr/bin/osascript");
    cmd.arg("-e").arg(TAB_SCRIPT).arg(cwd).arg(tab_command(argv.len()));
    cmd.args(argv.iter().enumerate().map(|(i, a)| format!("{ARG_VAR}{i}={a}")));
    // osascript prints the new tab's reference, and its errors mean "use a window instead".
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// The command line a Ghostty tab runs: variable names only, so nothing a ticket says is ever
/// part of it. `set --` rebuilds the argv from the variables, `unset` keeps them out of the
/// session's environment, and `exec` leaves no shell behind.
fn tab_command(n: usize) -> String {
    let names: Vec<String> = (0..n).map(|i| format!("{ARG_VAR}{i}")).collect();
    let refs: Vec<String> = names.iter().map(|v| format!("\"${v}\"")).collect();
    format!("/bin/sh -c 'set -- {}; unset {}; exec \"$@\"'", refs.join(" "), names.join(" "))
}

fn open_window(cwd: &str, argv: &[String], inherited: &[String]) -> Result<(), Error> {
    let mut cmd = Command::new("open");
    // Also keep them off `open` itself, in case Launch Services passes its environment on.
    for k in inherited {
        cmd.env_remove(k);
    }
    // `-n`: a new instance, so the session gets its own window.
    cmd.args(["-na", GHOSTTY, "--args"])
        .arg(format!("--working-directory={cwd}"))
        .arg("-e")
        .args(argv);
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

    /// What a tab runs, run the way Ghostty runs it: the arguments must come out byte for byte,
    /// however hostile, and the carrier variables must not reach the program.
    #[test]
    fn a_tab_passes_ticket_text_through_untouched() {
        let hostile = "line one\nline two $(touch /tmp/ai-kanban-pwned) `id` ' \" ; x=1 $HOME";
        let argv = ["/bin/sh".to_string(), "-c".into(), "printf '%s|' \"$1\" \"${AI_KANBAN_ARG_3-unset}\"".into(),
            "_".into(), hostile.into()];
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(tab_command(argv.len()))
            .envs(argv.iter().enumerate().map(|(i, a)| (format!("{ARG_VAR}{i}"), a)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("{hostile}|unset|"));
        assert!(!Path::new("/tmp/ai-kanban-pwned").exists());
    }
}
