//! The plugin's own configuration, checked as a build artifact.
//!
//! These exist because the failure they guard is invisible in this repository. Every path
//! in the plugin resolves correctly here, where the plugin sits inside a checkout; it is
//! only a *copy* of the plugin -- the normal way to install one -- that breaks. Nothing in
//! the test suite exercised that, so the plugin shipped for four days pointing at a binary
//! it could only find in one directory on one machine.

use std::path::{Path, PathBuf};
use std::process::Command;

fn plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/skills/ai-kanban")
}

fn config(name: &str) -> String {
    std::fs::read_to_string(plugin_dir().join(name)).expect("plugin config must exist")
}

#[test]
fn no_plugin_config_reaches_outside_the_plugin_directory() {
    // `${CLAUDE_PLUGIN_ROOT}/../../../target/release/ai-kanban` is the shape that broke:
    // it only resolves when the plugin lives inside an ai-kanban checkout. Anything
    // climbing out of the plugin root is making the same assumption.
    for name in ["hooks.json", "mcp.json"] {
        let body = config(name);
        assert!(
            !body.contains(".."),
            "{name} escapes the plugin directory, so a copied plugin points at nothing"
        );
    }
}

#[test]
fn both_entry_points_go_through_the_resolver() {
    // The MCP server and the hooks have to agree. When only one was fixed in the past, the
    // board still appeared at session start while the tools were silently absent -- which
    // reads as "the agent ignored the board" rather than as a broken install.
    assert!(config("mcp.json").contains("${CLAUDE_PLUGIN_ROOT}/bin/ai-kanban"));
    let hooks = config("hooks.json");
    assert_eq!(
        hooks.matches("/bin/ai-kanban").count(),
        2,
        "both SessionStart and PostToolUse must resolve the binary the same way"
    );
}

#[test]
fn the_resolver_is_executable() {
    // A resolver committed without its executable bit fails exactly like a missing binary,
    // and git preserves only this one permission bit.
    let resolver = plugin_dir().join("bin/ai-kanban");
    let meta = std::fs::metadata(&resolver).expect("resolver must be committed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(meta.permissions().mode() & 0o111 != 0, "resolver is not executable");
    }
    let _ = meta;
}

#[test]
fn a_copied_plugin_that_finds_no_binary_stays_silent_on_hooks() {
    // The load-bearing half of `CLAUDE.md`'s "never fail loudly": this runs in every
    // directory the user opens Claude Code in, including ones with no Rust toolchain.
    if cfg!(not(unix)) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let copied = tmp.path().join("ai-kanban");
    let status = Command::new("cp").arg("-R").arg(plugin_dir()).arg(&copied).status().unwrap();
    assert!(status.success());

    let out = Command::new(copied.join("bin/ai-kanban"))
        .args(["hook", "session-start"])
        // An empty PATH and a HOME with nothing in it is the machine that has never built
        // this project -- the case a copied plugin actually lands on.
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", tmp.path().join("empty"))
        .env_remove("AI_KANBAN_BIN")
        .output()
        .unwrap();

    assert!(out.status.success(), "a hook that cannot find its binary must still exit 0");
    assert!(out.stdout.is_empty(), "silence means silence: {:?}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn a_copied_plugin_explains_itself_for_everything_that_is_not_a_hook() {
    // The mirror of the test above. Silence is right for hooks and wrong here: an MCP
    // server that exits without a word is a board that is mysteriously missing.
    if cfg!(not(unix)) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let copied = tmp.path().join("ai-kanban");
    Command::new("cp").arg("-R").arg(plugin_dir()).arg(&copied).status().unwrap();

    let out = Command::new(copied.join("bin/ai-kanban"))
        .arg("mcp")
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", tmp.path().join("empty"))
        .env_remove("AI_KANBAN_BIN")
        .output()
        .unwrap();

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("AI_KANBAN_BIN"), "the message must name the way out: {err}");
}

#[test]
fn an_explicit_binary_override_wins() {
    // AI_KANBAN_BIN is the escape hatch for every layout the search order does not know
    // about -- including Windows, where the resolver itself does not run.
    if cfg!(not(unix)) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let copied = tmp.path().join("ai-kanban");
    Command::new("cp").arg("-R").arg(plugin_dir()).arg(&copied).status().unwrap();

    let out = Command::new(copied.join("bin/ai-kanban"))
        .arg("where")
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", tmp.path().join("empty"))
        .env("AI_KANBAN_BIN", "/bin/echo")
        .output()
        .unwrap();

    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "where", "the override was not used");
}

#[test]
fn the_documented_install_location_is_found_without_help_from_path() {
    // README tells people to install into a directory on their PATH, and recommends
    // ~/.local/bin because it needs no sudo. A hook is not a login shell, though -- the host
    // may invoke it with a minimal PATH that contains none of the user's directories. So the
    // recommended location has to be searched by name, not left to PATH.
    //
    // This is the test that fails if someone trims the resolver's candidate list: the
    // symptom otherwise is a plugin that works when you type the command yourself and
    // silently does nothing in a session, which is close to undiagnosable.
    if cfg!(not(unix)) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let local_bin = home.join(".local/bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    // A stub rather than a copy of a real system binary: on macOS, copying a signed system
    // executable and running the copy is killed by the signature check, with no stderr.
    let stub = local_bin.join("ai-kanban");
    std::fs::write(&stub, "#!/bin/sh\necho \"$@\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let copied = tmp.path().join("plugin");
    Command::new("cp").arg("-R").arg(plugin_dir()).arg(&copied).status().unwrap();

    let out = Command::new(copied.join("bin/ai-kanban"))
        .arg("where")
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env_remove("AI_KANBAN_BIN")
        .output()
        .unwrap();

    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "where",
        "~/.local/bin is the install location the README recommends and was not searched"
    );
}
