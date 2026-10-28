//! `/ai-kanban:install-cli` end to end with the real binary: the MCP server sets up the link in
//! the plugin's data directory, `install-cli` links the terminal to it, and the command then
//! runs by name through both links. See `src/install_cli.rs` for why it takes two.

#![cfg(unix)]

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ai-kanban");

#[test]
fn the_terminal_reaches_the_plugins_binary_through_the_data_link() {
    let tmp = tempfile::tempdir().unwrap();
    let (data, store, bin) = (tmp.path().join("plugin-data"), tmp.path().join("kanban.db"), tmp.path().join("local-bin"));

    // A session starts: the MCP server, with stdin closed, sets up the link and ends.
    let out = Command::new(BIN)
        .arg("mcp")
        .env("CLAUDE_PLUGIN_DATA", &data)
        .env("AI_KANBAN_DB", &store)
        .env("AI_KANBAN_SYNC", "off")
        .env_remove("AI_KANBAN_PLUGIN_RELAY_URL")
        .env_remove("AI_KANBAN_PLUGIN_RELAY_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stable = data.join("bin/ai-kanban");
    assert!(std::fs::read_link(&stable).is_ok(), "the server links its binary: {}", String::from_utf8_lossy(&out.stderr));

    let out = Command::new(BIN).args(["install-cli", "--link"]).arg(&stable).arg("--dir").arg(&bin).env("PATH", "/usr/bin:/bin").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("linked") && text.contains("is not on your PATH"), "{text}");

    let out = Command::new(bin.join("ai-kanban")).arg("where").env("AI_KANBAN_DB", &store).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), store.to_str().unwrap());
}

#[test]
fn install_cli_refuses_to_overwrite_and_says_what_is_there() {
    let tmp = tempfile::tempdir().unwrap();
    let stable = tmp.path().join("data/bin/ai-kanban");
    std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(BIN, &stable).unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("ai-kanban"), b"old copy").unwrap();

    let out = Command::new(BIN).args(["install-cli", "--link"]).arg(&stable).arg("--dir").arg(&bin).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("a file of 8 bytes") && err.contains("--replace"), "{err}");
    assert_eq!(std::fs::read(bin.join("ai-kanban")).unwrap(), b"old copy");
}
