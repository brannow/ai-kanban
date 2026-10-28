//! The plugin's own configuration, checked from an installed **copy**.
//!
//! # Why these exist
//!
//! The failure they guard is invisible from inside this repository. The plugin once shipped
//! for four days pointing at `${CLAUDE_PLUGIN_ROOT}/../../../target/release/ai-kanban`, which
//! resolves only when the plugin sits inside an ai-kanban checkout. Every path worked here,
//! where it was the only place anyone ran it; only a *copy* broke, and copying is how a plugin
//! is installed. See note #15 on the board.
//!
//! The plugin now names its binary as `${CLAUDE_PLUGIN_ROOT}/bin/ai-kanban`, a wrapper that
//! runs `../libexec/ai-kanban-<os>-<arch>` from its own location -- the layout the release zip,
//! `make install` and `make install-dev` all produce. So these tests install through
//! `make plugin` into a temporary directory and run what the host would run, from `/`, with a
//! bare PATH and an empty HOME: nothing of this checkout in reach.
//!
//! They drive `make plugin` rather than `make install` on purpose: the layout is where the
//! bugs are, and shelling out to `cargo build --release` from inside `cargo test` is slow and
//! serialises on the build lock for no added coverage. A stub stands in for the binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Installs the plugin into `claude_dir` with `bin` as its binary. Returns the plugin directory.
fn install(claude_dir: &Path, bin: &Path) -> PathBuf {
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

/// An executable stand-in that says which arguments it got, so a test can tell it ran.
fn stub(dir: &Path) -> PathBuf {
    let p = dir.join("ai-kanban");
    std::fs::write(&p, "#!/bin/sh\necho \"stub $*\"\n").unwrap();
    executable(&p);
    p
}

fn executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn read(plugin: &Path, name: &str) -> String {
    std::fs::read_to_string(plugin.join(name)).unwrap_or_else(|e| panic!("{name} must be installed: {e}"))
}

fn json(plugin: &Path, name: &str) -> serde_json::Value {
    serde_json::from_str(&read(plugin, name)).unwrap_or_else(|e| panic!("{name} must be valid JSON: {e}"))
}

/// A command the way the host runs it: `${CLAUDE_PLUGIN_ROOT}` substituted, through `sh`, from
/// `/`, with a bare PATH (plus `extra_path` first, to fake tools) and an empty HOME.
fn run(plugin: &Path, command: &str, extra_path: Option<&Path>) -> Output {
    let cmd = command.replace("${CLAUDE_PLUGIN_ROOT}", plugin.to_str().unwrap());
    let path = match extra_path {
        Some(p) => format!("{}:/usr/bin:/bin", p.display()),
        None => "/usr/bin:/bin".into(),
    };
    let home = tempfile::tempdir().unwrap();
    Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir("/")
        .env_clear()
        .env("PATH", path)
        .env("HOME", home.path())
        .env("CLAUDE_PLUGIN_ROOT", plugin)
        .output()
        .unwrap()
}

fn hook_command(plugin: &Path, event: &str) -> String {
    json(plugin, "hooks/hooks.json")["hooks"][event][0]["hooks"][0]["command"].as_str().expect("a command").to_string()
}

fn mcp_command(plugin: &Path) -> String {
    let server = &json(plugin, ".mcp.json")["mcpServers"]["ai-kanban"];
    let args: Vec<&str> = server["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    format!("\"{}\" {}", server["command"].as_str().unwrap(), args.join(" "))
}

#[test]
fn the_configs_name_the_binary_through_the_plugin_root_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));

    for name in ["hooks/hooks.json", ".mcp.json"] {
        let text = read(&plugin, name);
        assert!(text.contains("${CLAUDE_PLUGIN_ROOT}/bin/ai-kanban"), "{name} must run the wrapper: {text}");
        // The bug this file exists for: a hop out of the plugin points at nothing once copied.
        assert!(!text.contains(".."), "{name} must not leave the plugin directory: {text}");
        assert!(!text.contains("@BIN@") && !text.contains(tmp.path().to_str().unwrap()),
            "{name} must not carry an install-time path: {text}");
    }
}

#[test]
fn the_manifest_leaves_versions_to_the_release() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));
    let manifest = json(&plugin, ".claude-plugin/plugin.json");
    assert_eq!(manifest["name"], "ai-kanban");
    // A pinned version wins over the archive's digest, so every release would read as the same
    // version and no user would ever be offered an update.
    assert!(manifest.get("version").is_none(), "{manifest}");
    // Default locations: naming them as well would load the same file twice.
    assert!(manifest.get("hooks").is_none() && manifest.get("mcpServers").is_none(), "{manifest}");
}

#[test]
fn every_entry_point_runs_the_installed_binary_from_an_unrelated_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));

    for (event, arg) in [("SessionStart", "hook session-start"), ("PostToolUse", "hook post-tool-use")] {
        let out = run(&plugin, &hook_command(&plugin, event), None);
        assert!(out.status.success(), "{event}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("stub {arg}"), "{event} must reach the binary");
    }
    let out = run(&plugin, &mcp_command(&plugin), None);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "stub mcp", "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_missing_binary_leaves_the_hooks_silent_but_not_the_mcp_server() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));
    for entry in std::fs::read_dir(plugin.join("libexec")).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }

    // Hook code must never fail loudly (CLAUDE.md): a hook that complains on every session is a
    // hook the user deletes, and deleting it takes the bundled MCP server with it.
    for event in ["SessionStart", "PostToolUse"] {
        let out = run(&plugin, &hook_command(&plugin, event), None);
        assert!(out.status.success(), "{event} must still exit 0 when the binary is gone");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{event} must say nothing: {out:?}");
    }
    // Deliberately asymmetric: an MCP server that dies silently is a board that is mysteriously
    // absent, which is harder to diagnose than a startup error.
    let out = run(&plugin, &mcp_command(&plugin), None);
    assert!(!out.status.success() && !out.stderr.is_empty(), "the MCP server must fail visibly: {out:?}");
}

#[test]
fn an_unsupported_machine_is_named_by_the_wrapper_and_swallowed_by_the_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));
    let fake = tmp.path().join("fakebin");
    std::fs::create_dir(&fake).unwrap();
    let uname = fake.join("uname");
    std::fs::write(&uname, "#!/bin/sh\ncase \"$1\" in -s) echo SunOS ;; -m) echo sparc ;; esac\n").unwrap();
    executable(&uname);

    let out = run(&plugin, &mcp_command(&plugin), Some(&fake));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no build for SunOS sparc"), "{out:?}");
    let out = run(&plugin, &hook_command(&plugin, "SessionStart"), Some(&fake));
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

#[test]
fn a_dev_install_runs_the_checkouts_build_through_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let out = Command::new("make")
        .args(["plugin", "PLUGIN_LINK=1"])
        .arg(format!("CLAUDE_DIR={}", tmp.path().display()))
        .arg(format!("PLUGIN_BIN={}", bin.display()))
        .current_dir(repo())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let plugin = tmp.path().join("skills/ai-kanban");
    // A rebuild must reach the plugin without reinstalling it.
    std::fs::write(&bin, "#!/bin/sh\necho rebuilt\n").unwrap();
    let out = run(&plugin, &mcp_command(&plugin), None);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "rebuilt");
}

#[test]
fn installing_into_a_private_claude_directory_touches_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = stub(tmp.path());
    let private = tmp.path().join(".claude-private");
    let plugin = install(&private, &bin);

    assert!(plugin.starts_with(&private), "the plugin must land under CLAUDE_DIR");
    assert!(plugin.join("hooks/hooks.json").exists());
    assert!(!tmp.path().join(".claude").exists(), "a private install must not also write to the default directory");
}

#[test]
fn the_hooks_file_uses_only_top_level_keys_the_loader_accepts() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));

    // The loader accepts exactly these four and warns on anything else, once per session
    // start: `ai-kanban: hooks.json: unknown key "x" ignored`. The hooks still run, so this
    // never fails a test by itself -- it just makes the plugin noisy, and noisy is what gets
    // uninstalled.
    //
    // A test rather than a CI `claude plugin validate` step: validate checks the manifest, not
    // this file's top-level keys. It passes a `hooks.json` full of junk keys.
    //
    // We shipped `"_comment"` and warned on every session start until someone noticed.
    const ALLOWED: [&str; 4] = ["description", "hooks", "modules", "surface"];

    let hooks = json(&plugin, "hooks/hooks.json");
    let obj = hooks.as_object().expect("hooks.json must be a JSON object");
    for key in obj.keys() {
        assert!(ALLOWED.contains(&key.as_str()),
            "hooks.json top-level key {key:?} makes the loader warn on every session start; allowed: {ALLOWED:?}");
    }
    assert!(obj.contains_key("description"), "the plugin should say what its hooks do");

    // Matcher entries allow only these two -- and `description` being legal one level up is
    // exactly what makes it plausible for someone to add one down here.
    let events = hooks["hooks"].as_object().expect("hooks must be an object of events");
    for (event, entries) in events {
        for entry in entries.as_array().expect("an event maps to an array of matcher entries") {
            for key in entry.as_object().expect("a matcher entry is an object").keys() {
                assert!(matches!(key.as_str(), "matcher" | "hooks"),
                    "hooks.{event}[..] key {key:?} makes the loader warn; allowed: matcher, hooks");
            }
        }
    }
}

#[test]
fn the_relay_settings_reach_the_mcp_server_and_the_key_stays_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = install(tmp.path(), &stub(tmp.path()));
    let manifest = json(&plugin, ".claude-plugin/plugin.json");
    let options = manifest["userConfig"].as_object().expect("the manifest declares the relay settings");

    // A reference to an undeclared option is dropped by the loader, and the setting would then
    // never reach the server -- sync silently off for whoever filled it in.
    let env = json(&plugin, ".mcp.json")["mcpServers"]["ai-kanban"]["env"].clone();
    for (var, value) in env.as_object().expect("the server gets the settings through env") {
        let name = value.as_str().unwrap().strip_prefix("${user_config.").and_then(|v| v.strip_suffix('}'))
            .unwrap_or_else(|| panic!("{var} must come from a setting: {value}"));
        assert!(options.contains_key(name), "{var} names an undeclared setting {name:?}");
    }
    assert_eq!(options["relay_key"]["sensitive"], true, "the key belongs in the keychain, not in settings.json");
    for (name, o) in options {
        assert_ne!(o["required"], true, "{name}: most installs are local and must be able to leave it empty");
    }
}
