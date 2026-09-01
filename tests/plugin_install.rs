//! The plugin's own configuration, checked as a **generated** artifact.
//!
//! # Why these exist
//!
//! The failure they guard is invisible from inside this repository. The previous plugin
//! shipped for four days pointing at
//! `${CLAUDE_PLUGIN_ROOT}/../../../target/release/ai-kanban`, which resolves only when the
//! plugin sits inside an ai-kanban checkout. Every path worked here, where it was the only
//! place anyone ran it; only a *copy* broke, and copying is how a plugin is installed.
//! See note #15 on the board.
//!
//! `make install` now generates the config with the binary's absolute path baked in, so
//! there is no relative resolution left to get wrong. That removes the old bug class and
//! introduces a smaller one: the generation itself. These tests are aimed at that, and they
//! install into a temporary prefix rather than reading anything out of the working tree.
//!
//! They drive `make plugin` rather than `make install` on purpose — the generation is where
//! the bugs are, and shelling out to `cargo build --release` from inside `cargo test` is
//! slow and serialises on the build lock for no added coverage.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Renders the plugin into `claude_dir`, pointed at `bin`. Returns the plugin directory.
fn generate(claude_dir: &Path, bin: &Path) -> PathBuf {
    let out = Command::new("make")
        .arg("plugin")
        .arg(format!("CLAUDE_DIR={}", claude_dir.display()))
        .arg(format!("PLUGIN_BIN={}", bin.display()))
        .current_dir(repo())
        .output()
        .expect("make must be available");
    assert!(out.status.success(), "make plugin failed: {}", String::from_utf8_lossy(&out.stderr));
    claude_dir.join("skills/ai-kanban")
}

/// An executable stand-in, so these tests never need a release build.
fn stub(dir: &Path) -> PathBuf {
    let p = dir.join("ai-kanban");
    std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    p
}

fn read(plugin: &Path, name: &str) -> String {
    std::fs::read_to_string(plugin.join(name))
        .unwrap_or_else(|e| panic!("{name} must be generated: {e}"))
}

/// The hook command as the host would run it: through a shell, from an unrelated directory.
fn run_hook(plugin: &Path, event: &str) -> std::process::Output {
    let hooks: serde_json::Value = serde_json::from_str(&read(plugin, "hooks.json")).unwrap();
    let key = if event == "session-start" { "SessionStart" } else { "PostToolUse" };
    let cmd = hooks["hooks"][key][0]["hooks"][0]["command"].as_str().expect("a command").to_string();
    Command::new("sh").arg("-c").arg(&cmd).current_dir("/").output().unwrap()
}

#[test]
fn the_generated_config_names_the_binary_by_absolute_path() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let plugin = generate(tmp.path(), &bin);

    for name in ["hooks.json", "mcp.json"] {
        let text = read(&plugin, name);
        assert!(text.contains(bin.to_str().unwrap()), "{name} must name the binary: {text}");
        // The bug this whole file exists for. A plugin is installed by being copied, so
        // anything resolved relative to the plugin's own location points at nothing.
        assert!(!text.contains("CLAUDE_PLUGIN_ROOT"),
            "{name} must not resolve relative to the plugin directory: {text}");
        assert!(!text.contains(".."), "{name} must not contain a relative hop: {text}");
    }
}

#[test]
fn the_generated_config_is_valid_json_and_wires_both_entry_points() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let plugin = generate(tmp.path(), &bin);

    let manifest: serde_json::Value =
        serde_json::from_str(&read(&plugin, ".claude-plugin/plugin.json")).unwrap();
    assert_eq!(manifest["name"], "ai-kanban");
    assert_eq!(manifest["hooks"], "./hooks.json");
    assert_eq!(manifest["mcpServers"], "./mcp.json");

    let hooks: serde_json::Value = serde_json::from_str(&read(&plugin, "hooks.json")).unwrap();
    assert!(hooks["hooks"]["SessionStart"].is_array(), "the board must reach a cold session");
    assert!(hooks["hooks"]["PostToolUse"].is_array(), "notes must reach a touched file");

    let mcp: serde_json::Value = serde_json::from_str(&read(&plugin, "mcp.json")).unwrap();
    assert_eq!(mcp["mcpServers"]["ai-kanban"]["command"], bin.to_str().unwrap());
    assert_eq!(mcp["mcpServers"]["ai-kanban"]["args"][0], "mcp");
}

#[test]
fn the_generated_hook_runs_from_an_unrelated_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let plugin = generate(tmp.path(), &bin);

    // From `/`, with nothing of this checkout in reach -- the situation an installed plugin
    // is always in and the one the old relative path could not survive.
    for event in ["session-start", "post-tool-use"] {
        let out = run_hook(&plugin, event);
        assert!(out.status.success(), "{event} hook failed: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn a_missing_binary_leaves_the_hooks_silent_rather_than_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let plugin = generate(tmp.path(), &bin);
    // The binary moves or is uninstalled while the plugin stays. Hook code must never fail
    // loudly (CLAUDE.md): a hook that complains on every session is a hook the user
    // deletes, and deleting it takes the bundled MCP server with it.
    std::fs::remove_file(&bin).unwrap();

    for event in ["session-start", "post-tool-use"] {
        let out = run_hook(&plugin, event);
        assert!(out.status.success(), "{event} must still exit 0 when the binary is gone");
        assert!(out.stderr.is_empty(), "{event} must say nothing: {:?}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "{event} must emit no context: {:?}", String::from_utf8_lossy(&out.stdout));
    }
}

#[test]
fn the_mcp_server_is_not_silenced_the_way_the_hooks_are() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let plugin = generate(tmp.path(), &bin);
    let mcp = read(&plugin, "mcp.json");
    // Deliberately asymmetric with the hooks above. An MCP server that dies silently is a
    // board that is mysteriously absent, which is harder to diagnose than a startup error.
    assert!(!mcp.contains("|| true"), "mcp.json must not swallow a startup failure: {mcp}");
}

#[test]
fn installing_into_a_private_claude_directory_touches_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let private = tmp.path().join(".claude-private");
    let plugin = generate(&private, &bin);

    assert!(plugin.starts_with(&private), "the plugin must land under CLAUDE_DIR");
    assert!(plugin.join("hooks.json").exists());
    assert!(!tmp.path().join(".claude").exists(),
        "a private install must not also write to the default directory");
}
