//! `ai-kanban remote` and `ai-kanban room`: connecting this machine to a relay, and a board to
//! a room. A person's commands, never the agent's -- connecting publishes a board's whole
//! history, and that is a decision. `docs/sync.md`, "Connecting a board", is the design.
//!
//! Every question has a flag that answers it. Without a terminal (a script, `tests/cli.rs`) a
//! missing answer is an error that names the flag, and nothing is left half-done.

use super::client::{Client, RemoteError};
use super::config::{self, RemoteConfig};
use super::engine::{check_identity, sync_board};
use crate::core::Store;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

type CliResult = Result<(), Box<dyn std::error::Error>>;

/// Long enough for a relay on the other side of the world, short enough that a typo in the
/// URL fails while the person is still looking.
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct Args {
    pos: Vec<String>,
    key: Option<String>,
    board: Option<String>,
    move_to: Option<String>,
    disconnect: bool,
    yes: bool,
    keep_foreign: bool,
    wipe_foreign: bool,
    sync_first: bool,
    discard: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(2);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--key" => a.key = Some(value("--key")?),
            "--board" => a.board = Some(value("--board")?),
            "--move" => a.move_to = Some(value("--move")?),
            "--disconnect" => a.disconnect = true,
            "--yes" => a.yes = true,
            "--keep-foreign" => a.keep_foreign = true,
            "--wipe-foreign" => a.wipe_foreign = true,
            "--sync-first" => a.sync_first = true,
            "--discard" => a.discard = true,
            s if s.starts_with("--") => return Err(format!("unknown flag {s}")),
            _ => a.pos.push(arg),
        }
    }
    if a.keep_foreign && a.wipe_foreign {
        return Err("--keep-foreign and --wipe-foreign contradict each other".into());
    }
    if a.sync_first && a.discard {
        return Err("--sync-first and --discard contradict each other".into());
    }
    Ok(a)
}

/// Asks on the terminal, or fails naming the flag that answers it.
fn ask(question: &str, flag: &str) -> Result<String, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() {
        return Err(format!("{question}\nno terminal to ask on: pass {flag}").into());
    }
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_ascii_lowercase())
}

fn confirm(question: &str, yes: bool) -> Result<bool, Box<dyn std::error::Error>> {
    if yes {
        return Ok(true);
    }
    Ok(matches!(ask(&format!("{question} [y/N]:"), "--yes")?.as_str(), "y" | "yes"))
}

fn client(cfg: &RemoteConfig) -> Client {
    Client::new(&cfg.url, &cfg.key, TIMEOUT)
}

fn need_remote() -> Result<RemoteConfig, Box<dyn std::error::Error>> {
    RemoteConfig::load()?.ok_or_else(|| "no remote set. Run: ai-kanban remote <url> --key <key>".into())
}

fn need_network() -> CliResult {
    if config::sync_enabled() {
        Ok(())
    } else {
        Err("AI_KANBAN_SYNC is off for this run, so nothing can reach the relay".into())
    }
}

// ---------------------------------------------------------------------------
// ai-kanban remote
// ---------------------------------------------------------------------------

pub fn remote_cmd() -> CliResult {
    let a = args()?;
    let store = Store::open_default()?;
    if a.disconnect {
        return remote_disconnect(&store, &a);
    }
    if let Some(url) = &a.move_to {
        return remote_move(&store, url);
    }
    match (a.pos.first(), &a.key) {
        (None, None) => remote_show(&store),
        (Some(url), Some(key)) => remote_set(&store, url, key),
        _ => Err("usage: ai-kanban remote <url> --key <key>   |   ai-kanban remote   |   ai-kanban remote --move <url>   |   ai-kanban remote --disconnect".into()),
    }
}

fn remote_show(store: &Store) -> CliResult {
    let Some(cfg) = RemoteConfig::load()? else {
        println!("No remote set. Connect this machine to a relay with:\n\n  ai-kanban remote <url> --key <key>");
        return Ok(());
    };
    println!("remote   {}", cfg.url);
    println!("key      {}", cfg.masked_key());
    println!("tag      {}", store.origin_tag()?.unwrap_or_else(|| "none yet".into()));
    if !config::sync_enabled() {
        println!("sync     off for this run (AI_KANBAN_SYNC)");
        return Ok(());
    }
    let c = client(&cfg);
    match c.hello().and_then(|h| c.whoami().map(|w| (h, w))) {
        Ok((h, w)) => println!("relay    {} as {} (tag {}), protocol {}-{}", h.server_version, w.user, w.tag, h.protocol_min, h.protocol_max),
        Err(e) => println!("relay    {e}"),
    }
    if let Some(why) = store.paused()? {
        println!("\nsync paused: {why}");
    }
    Ok(())
}

fn remote_set(store: &Store, url: &str, key: &str) -> CliResult {
    need_network()?;
    let url = config::normalize_url(url);
    let connected = store.connected_boards()?;
    if let Some(old) = RemoteConfig::load()? {
        if old.url != url && !connected.is_empty() {
            return Err(format!(
                "{} board(s) are connected to rooms on {}. Disconnect them first (ai-kanban room disconnect <room>), \
                 or keep the relay and use: ai-kanban remote --move <url>",
                connected.len(), old.url
            ).into());
        }
    }
    let c = Client::new(&url, key, TIMEOUT);
    let hello = c.hello()?;
    let who = c.whoami()?;
    if let Some(tag) = store.origin_tag()? {
        if tag != who.tag && !connected.is_empty() {
            return Err(format!(
                "this store publishes as {tag} on {} connected board(s), and this key is for {}. Disconnect them first, \
                 then set the new key (work published as {tag} stays this store's own)",
                connected.len(), who.tag
            ).into());
        }
    }
    store.set_origin_tag(&who.tag)?;
    let cfg = RemoteConfig { url: url.clone(), key: key.trim().into() };
    cfg.save()?;
    println!("ok: logged in as {}, origin tag {} (protocol {}, relay {})", who.user, who.tag, hello.protocol_max, hello.server_version);
    if let Err(RemoteError::Paused(why)) = check_identity(store, &c) {
        println!("\nsync paused: {why}");
    } else if connected.is_empty() {
        println!("\nNext: ai-kanban room list, then ai-kanban room connect <room>");
    }
    Ok(())
}

fn remote_move(store: &Store, url: &str) -> CliResult {
    need_network()?;
    let mut cfg = need_remote()?;
    let url = config::normalize_url(url);
    let c = Client::new(&url, &cfg.key, TIMEOUT);
    let who = c.whoami()?;
    if store.origin_tag()?.as_deref() != Some(who.tag.as_str()) {
        return Err(format!("{url} knows this key as tag {}, not as this store's. That is a different relay, not a move", who.tag).into());
    }
    cfg.url = url;
    cfg.save()?;
    println!("ok: the relay is now reached at {}", cfg.url);
    Ok(())
}

fn remote_disconnect(store: &Store, a: &Args) -> CliResult {
    let cfg = need_remote()?;
    let boards = store.connected_boards()?;
    if !confirm(&format!("All connected boards will be uncoupled from {} and the key removed. Continue?", cfg.url), a.yes)? {
        println!("cancelled.");
        return Ok(());
    }
    for (pid, room) in boards {
        disconnect(store, &cfg, pid, &room, a)?;
    }
    RemoteConfig::remove()?;
    println!("remote removed. Every board is local again.");
    Ok(())
}

// ---------------------------------------------------------------------------
// ai-kanban room
// ---------------------------------------------------------------------------

pub fn room_cmd() -> CliResult {
    let a = args()?;
    let store = Store::open_default()?;
    match a.pos.first().map(String::as_str) {
        Some("list") | None => room_list(&store),
        Some("connect") => {
            let room = a.pos.get(1).ok_or("usage: ai-kanban room connect <room> [--board <b>] [--yes]")?;
            room_connect(&store, room, &a)
        }
        Some("disconnect") => {
            let room = a.pos.get(1).ok_or("usage: ai-kanban room disconnect <room> [--keep-foreign|--wipe-foreign] [--sync-first|--discard] [--yes]")?;
            let cfg = need_remote()?;
            let room = crate::sync::protocol::room_name(room).ok_or("not a room name")?;
            let pid = store.board_of_room(&room)?.ok_or_else(|| format!("no board on this machine is connected to room {room}"))?;
            disconnect(&store, &cfg, pid, &room, &a)
        }
        Some("status") => room_status(&store),
        Some("sync") => room_sync(&store),
        Some(other) => Err(format!("unknown room command {other:?}: list, connect, disconnect, status, sync").into()),
    }
}

fn room_list(store: &Store) -> CliResult {
    need_network()?;
    let cfg = need_remote()?;
    let rooms = client(&cfg).rooms()?;
    if rooms.is_empty() {
        println!("No rooms on {} yet. Connecting a board creates one: ai-kanban room connect <room>", cfg.url);
        return Ok(());
    }
    for r in rooms {
        let members = if r.members.is_empty() {
            "nobody connected".to_string()
        } else {
            r.members.iter().map(|m| format!("{} {}", m.tag, m.user)).collect::<Vec<_>>().join(", ")
        };
        let here = match store.board_of_room(&r.name)? {
            Some(pid) => format!("   <- board {}", store.project(pid)?.name),
            None => String::new(),
        };
        println!("{:<20} {:>6} changes   {members}{here}", r.name, r.changes);
    }
    Ok(())
}

fn room_connect(store: &Store, room: &str, a: &Args) -> CliResult {
    need_network()?;
    let cfg = need_remote()?;
    let room = crate::sync::protocol::room_name(room)
        .ok_or_else(|| format!("{room:?} is not a room name: lowercase letters, digits, '-' and '_'"))?;
    let c = client(&cfg);
    if let Err(e) = check_identity(store, &c) {
        return Err(e.into());
    }
    if let Some(pid) = store.board_of_room(&room)? {
        return Err(format!("room {room} is already connected to board {} on this machine", store.project(pid)?.name).into());
    }

    let pid = match &a.board {
        Some(b) => store.project_by_name_or_key(b)?.id,
        None => pick_board(store, &room)?,
    };
    let board = store.project(pid)?;
    if let Some(r) = store.room_of(pid)? {
        return Err(format!("board {} is already connected to room {r}", board.name).into());
    }

    let existing = c.rooms()?.into_iter().find(|r| r.name == room);
    let publish = store.publish_summary(pid)?;
    let tag = store.origin_tag()?.unwrap_or_default();
    println!("board \"{}\" -> room \"{room}\" on {}", board.name, cfg.url);
    println!("  will publish:  {} tasks, {} events, {} notes   (as #N-{tag})", publish.tasks, publish.events, publish.notes);
    match &existing {
        Some(r) => {
            let who = r.members.iter().map(|m| format!("{} ({})", m.user, m.tag)).collect::<Vec<_>>().join(", ");
            println!("  will receive:  {} changes{}", r.changes, if who.is_empty() { String::new() } else { format!(" from {who}") });
        }
        None => println!("  room \"{room}\" does not exist yet -- it will be created"),
    }
    if !confirm("Connect?", a.yes)? {
        println!("cancelled.");
        return Ok(());
    }

    let backup = Store::default_path()?
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default()
        .join(format!("kanban-before-connect-{room}-{}.db", crate::core::now()));
    store.backup_to(&backup)?;
    println!("backup written: {}", backup.display());

    c.join(&room)?;
    store.connect_board(pid, &room)?;
    match sync_board(store, &c, pid) {
        Ok(s) => println!("connected. Published {} changes, received {}. The board now shows as: Board: {} @{room}", s.pushed, s.applied, board.name),
        Err(e) => println!("connected, but the first sync failed: {e}\nIt is retried on the next sync; nothing is lost."),
    }
    Ok(())
}

/// The board to connect, asked for when `--board` was not given. Boards already in a room are
/// not offered, and the one this folder resolves to comes first.
fn pick_board(store: &Store, room: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let connected: Vec<i64> = store.connected_boards()?.into_iter().map(|(p, _)| p).collect();
    let here = std::env::current_dir().ok().and_then(|d| store.find_project(&d).ok().flatten()).map(|p| p.id);
    let mut boards: Vec<_> = store.all_projects()?.into_iter().filter(|p| !connected.contains(&p.id)).collect();
    if boards.is_empty() {
        return Err("every board on this machine is already connected to a room".into());
    }
    boards.sort_by_key(|p| (Some(p.id) != here, p.name.to_ascii_lowercase()));
    if !std::io::stdin().is_terminal() {
        return Err(format!("which board? pass --board <name> (one of: {})", boards.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")).into());
    }
    println!("Connect which local board to room \"{room}\"?");
    for (i, p) in boards.iter().enumerate() {
        let mark = if Some(p.id) == here { "   (this folder)" } else { "" };
        println!("  {}) {}{mark}", i + 1, p.name);
    }
    let answer = ask("Board [1]:", "--board <name>")?;
    let n: usize = if answer.is_empty() { 1 } else { answer.parse().map_err(|_| format!("{answer:?} is not one of the numbers"))? };
    boards.get(n.wrapping_sub(1)).map(|p| p.id).ok_or_else(|| format!("{n} is not one of the numbers").into())
}

fn disconnect(store: &Store, cfg: &RemoteConfig, pid: i64, room: &str, a: &Args) -> CliResult {
    let board = store.project(pid)?;
    let status = store.sync_status(pid)?.unwrap_or_default();
    println!("Board \"{}\" and room \"{room}\" will be uncoupled.", board.name);

    // Unsent changes first: disconnecting silently would keep them from the team.
    store.flush()?;
    let unsent = store.sync_status(pid)?.map(|s| s.unsent).unwrap_or(status.unsent);
    if unsent > 0 {
        let choice = if a.sync_first {
            "s".to_string()
        } else if a.discard {
            "d".to_string()
        } else {
            ask(&format!("  {unsent} local changes are not pushed yet.  [s]ync now / [d]iscard / [c]ancel:"), "--sync-first or --discard")?
        };
        match choice.as_str() {
            "s" | "sync" => {
                need_network()?;
                sync_board(store, &client(cfg), pid).map_err(|e| format!("sync failed, nothing was uncoupled: {e}"))?;
                println!("  synced.");
            }
            "d" | "discard" => {}
            _ => {
                println!("cancelled.");
                return Ok(());
            }
        }
    }

    let foreign = store.foreign_task_count(pid)?;
    let wipe = if foreign == 0 {
        false
    } else if a.wipe_foreign || a.keep_foreign {
        a.wipe_foreign
    } else {
        let answer = ask(
            &format!("  {foreign} tasks on this board came from other members. Wipe them locally? They stay in the room.  [k]eep / [w]ipe [k]:"),
            "--keep-foreign or --wipe-foreign",
        )?;
        matches!(answer.as_str(), "w" | "wipe")
    };
    if !confirm("  Uncouple?", a.yes)? {
        println!("cancelled.");
        return Ok(());
    }

    // Best effort: leaving tells the relay nobody here is connected any more. Offline, the board
    // is uncoupled anyway; the relay only uses membership for its own housekeeping.
    if config::sync_enabled() {
        if let Err(e) = client(cfg).leave(room) {
            println!("  (could not tell the relay: {e})");
        }
    }
    store.disconnect_board(pid, wipe)?;
    println!("disconnected. Board \"{}\" is local again{}.", board.name, if wipe { ", without other members' tasks" } else { "" });
    Ok(())
}

fn room_status(store: &Store) -> CliResult {
    let boards = store.connected_boards()?;
    if boards.is_empty() {
        println!("No board on this machine is connected to a room.");
        return Ok(());
    }
    if let Some(why) = store.paused()? {
        println!("sync paused: {why}\n");
    }
    if !config::sync_enabled() {
        println!("sync off for this run (AI_KANBAN_SYNC)\n");
    }
    for (pid, _) in boards {
        let s = store.sync_status(pid)?.unwrap_or_default();
        let when = s.last_ok_at.map(|t| format!("{}s ago", (crate::core::now() - t).max(0))).unwrap_or_else(|| "never".into());
        println!("{} @{}", store.project(pid)?.name, s.room);
        println!("  last sync   {when}");
        println!("  unsent      {}", s.unsent);
        if s.parked > 0 {
            println!("  parked      {} (need a newer ai-kanban)", s.parked);
        }
        if let Some(e) = &s.last_error {
            println!("  last error  {e}");
        }
        if !s.members.is_empty() {
            println!("  team        {}", s.members.iter().map(|(t, u)| format!("{t} {u}")).collect::<Vec<_>>().join(", "));
        }
    }
    Ok(())
}

fn room_sync(store: &Store) -> CliResult {
    need_network()?;
    let cfg = need_remote()?;
    let c = client(&cfg);
    check_identity(store, &c)?;
    for (pid, room) in store.connected_boards()? {
        match sync_board(store, &c, pid) {
            Ok(s) => println!("{room}: pushed {}, received {}{}", s.pushed, s.applied, if s.parked > 0 { format!(", parked {}", s.parked) } else { String::new() }),
            Err(e) => println!("{room}: {e}"),
        }
    }
    Ok(())
}
