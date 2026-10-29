//! `ai-kanban remote` and `ai-kanban room`: connecting this machine to a relay, and a board to
//! a room. A person's commands, never the agent's -- connecting publishes a board's whole
//! history, and that is a decision. `docs/sync.md`, "Connecting a board", is the design.
//!
//! Every question has a flag that answers it. Without a terminal (a script, `tests/cli.rs`) a
//! missing answer is an error that names the flag, and nothing is left half-done.

use super::client::{Client, RemoteError};
use super::config::{self, RemoteConfig};
use super::engine::{check_identity, sync_board};
use crate::core::model::BoardQuery;
use crate::core::Store;
use crate::sync::protocol::{RoomInfo, MAX_PAGE};
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
    name: Option<String>,
    board: Option<String>,
    move_to: Option<String>,
    disconnect: bool,
    yes: bool,
    keep_foreign: bool,
    wipe_foreign: bool,
    sync_first: bool,
    discard: bool,
    new_board: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(2);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--key" => a.key = Some(value("--key")?),
            "--name" => a.name = Some(value("--name")?),
            "--board" => a.board = Some(value("--board")?),
            "--move" => a.move_to = Some(value("--move")?),
            "--disconnect" => a.disconnect = true,
            "--yes" => a.yes = true,
            "--keep-foreign" => a.keep_foreign = true,
            "--wipe-foreign" => a.wipe_foreign = true,
            "--sync-first" => a.sync_first = true,
            "--discard" => a.discard = true,
            "--new-board" => a.new_board = true,
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
    Ok(ask_as_typed(question, flag)?.to_ascii_lowercase())
}

/// `ask`, keeping the case: for a name.
fn ask_as_typed(question: &str, flag: &str) -> Result<String, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() {
        return Err(format!("{question}\nno terminal to ask on: pass {flag}").into());
    }
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
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
    match (a.pos.first(), &a.key, &a.name) {
        (None, None, None) => remote_show(&store),
        (None, None, Some(name)) => remote_rename(name),
        (Some(url), Some(key), name) => remote_set(&store, url, key, name.as_deref()),
        _ => Err("usage: ai-kanban remote <url> --key <key> [--name <your name>]   |   ai-kanban remote   |   ai-kanban remote --name <new name>   |   ai-kanban remote --move <url>   |   ai-kanban remote --disconnect".into()),
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

/// What the team sees this machine as. The tag stays; only the name changes.
fn remote_rename(name: &str) -> CliResult {
    need_network()?;
    let cfg = need_remote()?;
    let who = client(&cfg).set_name(name).map_err(|e| match e {
        RemoteError::NotFound(_) | RemoteError::Http(405, _) => {
            format!("the relay at {} is older than names: it keeps the names its admin gave. Update the relay", cfg.url)
        }
        e => e.to_string(),
    })?;
    println!("ok: the team now sees you as {} (tag {}).", who.user, who.tag);
    if config::wanted_name().is_some() {
        println!("Your plugin settings name you too, and win at the next session start: change it there.");
    }
    Ok(())
}

fn remote_set(store: &Store, url: &str, key: &str, name: Option<&str>) -> CliResult {
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
    // Asked before anything else is sent: on a key not used yet, the first name fixes its tag.
    let asked;
    let setting = config::wanted_name();
    let name = match name.or(setting.as_deref()) {
        Some(n) => Some(n),
        None if std::io::stdin().is_terminal() => {
            asked = ask_as_typed("Your name, as the team sees you (Enter: keep yours, or get one like brave-otter):", "--name")?;
            Some(asked.as_str()).filter(|n| !n.is_empty())
        }
        None => None,
    };
    let who = c.identify(name)?;
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
    println!("The team sees you as {}; change it any time: ai-kanban remote --name <name>", who.user);
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
        Some("show") => {
            let room = a.pos.get(1).ok_or("usage: ai-kanban room show <room>")?;
            room_show(&store, room)
        }
        Some("connect") => {
            let room = a.pos.get(1).ok_or("usage: ai-kanban room connect <room> [--board <b> | --new-board] [--yes]")?;
            room_connect(&store, room, &a)
        }
        Some("disconnect") => {
            let room = a.pos.get(1).ok_or("usage: ai-kanban room disconnect <room> [--keep-foreign|--wipe-foreign] [--sync-first|--discard] [--yes]")?;
            let cfg = need_remote()?;
            let room = crate::sync::protocol::room_name(room).ok_or("not a room name")?;
            let pid = store.board_of_room(&room)?.ok_or_else(|| format!("no board on this machine is connected to room {room}"))?;
            disconnect(&store, &cfg, pid, &room, &a)
        }
        // `status` was the shared boards' half of what `list` now shows; kept for what notes say.
        Some("status") => room_list(&store),
        Some("sync") => room_sync(&store),
        Some(other) => Err(format!("unknown room command {other:?}: list, show, connect, disconnect, status, sync").into()),
    }
}

/// Every room on the relay, and for the ones a board here shares, how its sync is doing. One
/// command for both: what is out there and how mine are doing are the same question. Without
/// the relay, what this machine knows of its own shared boards.
fn room_list(store: &Store) -> CliResult {
    let cfg = need_remote()?;
    if let Some(why) = store.paused()? {
        println!("sync paused: {why}\n");
    }
    if !config::sync_enabled() {
        println!("sync off for this run (AI_KANBAN_SYNC)\n");
    }
    let now = crate::core::now();
    let rooms = if config::sync_enabled() { client(&cfg).rooms() } else { Err(RemoteError::Offline("sync is off for this run".into())) };
    let rooms = match rooms {
        Ok(r) => r,
        Err(e) => {
            println!("{} is not answering ({e}). What this machine knows of its shared boards:\n", cfg.url);
            for (pid, room) in store.connected_boards()? {
                let s = store.sync_status(pid)?.unwrap_or_default();
                println!("{room:<16} <- your board {}\n{:<16} {}", store.project(pid)?.name, "", crate::render::sync_state(&s, now));
            }
            return Ok(());
        }
    };
    if rooms.is_empty() {
        println!("No rooms on {} yet. Connecting a board creates one: ai-kanban room connect <room>", cfg.url);
        return Ok(());
    }
    for r in rooms {
        let mine = match store.board_of_room(&r.name)? {
            Some(pid) => Some((store.project(pid)?.name, store.sync_status(pid)?.unwrap_or_default())),
            None => None,
        };
        let line = crate::render::room_line(&r, mine.as_ref().map(|(b, s)| (b.as_str(), s)), now);
        println!("{}", crate::render::safe(&line));
    }
    println!("\nWhat is in one: ai-kanban room show <room>");
    Ok(())
}

fn room_show(store: &Store, room: &str) -> CliResult {
    need_network()?;
    let cfg = need_remote()?;
    let room = parse_room(room)?;
    let c = client(&cfg);
    let info = find_room(&c, &room)?.ok_or_else(|| format!("no room {room} on {}. All of them: ai-kanban room list", cfg.url))?;
    print!("{}", preview(&c, &cfg.url, &info)?);
    match store.board_of_room(&room)? {
        Some(pid) => println!("\nConnected here to your board {}.", store.project(pid)?.name),
        None => println!("\nConnect one of your boards to it: ai-kanban room connect {room}"),
    }
    Ok(())
}

fn parse_room(room: &str) -> Result<String, Box<dyn std::error::Error>> {
    crate::sync::protocol::room_name(room)
        .ok_or_else(|| format!("{room:?} is not a room name: lowercase letters, digits, '-' and '_'").into())
}

fn find_room(c: &Client, room: &str) -> Result<Option<RoomInfo>, RemoteError> {
    Ok(c.rooms()?.into_iter().find(|r| r.name == room))
}

/// The tag a preview store publishes under. The relay never issues it -- tags it hands out are
/// an initial and a number -- so no change in a room can look like the preview's own.
const PREVIEW_TAG: &str = "preview";

/// What a room holds, shown as the board view shows any board, without connecting to it: the
/// room's log pulled into a throwaway in-memory store and rendered from there. The relay
/// interprets nothing; the counts and claims are the client's own board logic.
///
/// Pull and apply only. `sync_board` would also push, sending the temporary board's own
/// creation into the room; `check_identity` would give the store this machine's tag, and this
/// machine's published tasks would then read as the preview's own.
fn preview(c: &Client, url: &str, info: &RoomInfo) -> Result<String, Box<dyn std::error::Error>> {
    Ok(look(c, url, info, None)?.0)
}

/// `preview`, also counting the tasks in the room created under one of `own`'s tags: what a
/// board of `own` published there.
fn look(c: &Client, url: &str, info: &RoomInfo, own: Option<&Store>) -> Result<(String, i64), Box<dyn std::error::Error>> {
    let mut out = crate::render::room_header(info, url, crate::core::now());
    if info.changes == 0 {
        return Ok((crate::render::safe(&out).into_owned(), 0));
    }
    let mut mine = std::collections::HashSet::new();
    let (tmp, pid) = Store::preview(&info.name, info.board.as_deref().unwrap_or(&info.name), PREVIEW_TAG)?;
    let mut since = 0;
    loop {
        let page = c.pull(&info.name, since, MAX_PAGE).map_err(|e| match e {
            // A relay from before reading was open to every key.
            RemoteError::NotFound(m) if m.contains("not a member") => {
                format!("the relay at {url} shows a room only to its members -- it is older than this ai-kanban. Update the relay")
            }
            e => e.to_string(),
        })?;
        let Some(last) = page.changes.iter().map(|p| p.seq).max() else { break };
        since = last;
        if let Some(store) = own {
            for p in &page.changes {
                if p.change.entity.kind == "task" && store.is_own_tag(&p.change.entity.origin)? {
                    mine.insert((p.change.entity.origin.clone(), p.change.entity.id));
                }
            }
        }
        tmp.apply(pid, &page.changes)?;
        if !page.more {
            break;
        }
    }
    let members: Vec<(String, String)> = info.members.iter().map(|m| (m.tag.clone(), m.user.clone())).collect();
    tmp.sync_succeeded(pid, &members)?;
    out.push('\n');
    out.push_str(&crate::render::board(&tmp.board(pid, &BoardQuery::board())?));
    // Everything in it came from the room: through the same filter as every answer.
    Ok((crate::render::safe(&out).into_owned(), mine.len() as i64))
}

/// Which board a room connects to: one that exists, or one made for it -- made only once the
/// person has confirmed, so cancelling leaves nothing behind.
enum Pick {
    Board(i64),
    New(String),
}

fn room_connect(store: &Store, room: &str, a: &Args) -> CliResult {
    need_network()?;
    let cfg = need_remote()?;
    let room = parse_room(room)?;
    let c = client(&cfg);
    if let Err(e) = check_identity(store, &c) {
        return Err(e.into());
    }
    if let Some(pid) = store.board_of_room(&room)? {
        return Err(format!("room {room} is already connected to board {} on this machine", store.project(pid)?.name).into());
    }

    // What is in the room comes first: whoever answers the questions below should know which
    // room they are answering them about.
    let existing = find_room(&c, &room)?;
    let mut published_before = 0;
    match &existing {
        // A relay from before open reads cannot show it; connecting must still work there.
        Some(info) => match look(&c, &cfg.url, info, Some(store)) {
            Ok((p, mine)) => {
                println!("{p}");
                published_before = mine;
            }
            Err(e) => println!("(cannot show what is in room \"{room}\": {e})\n"),
        },
        None => println!("Room \"{room}\" does not exist yet on {} -- connecting creates it.\n", cfg.url),
    }

    let pick = match (&a.board, a.new_board) {
        (Some(_), true) => return Err("--board and --new-board contradict each other".into()),
        (Some(b), false) => Pick::Board(store.project_by_name_or_key(b)?.id),
        (None, true) => Pick::New(free_name(store, &room)?),
        (None, false) => pick_board(store, &room)?,
    };
    // Before anything is written -- the backup, the join, a new board.
    let target = match &pick {
        Pick::Board(pid) => Some(*pid),
        Pick::New(_) => None,
    };
    if let Some((elsewhere, tasks)) = store.room_held_elsewhere(&room, target)? {
        let held = store.project(elsewhere)?.name;
        let fix = match &pick {
            Pick::Board(pid) => format!("or merge it into this one first: ai-kanban board merge \"{}\" \"{held}\"", store.project(*pid)?.name),
            Pick::New(_) => "or merge it into the board you want first (ai-kanban board merge <board> <that board>)".to_string(),
        };
        return Err(format!(
            "{tasks} task(s) of room {room} are on board \"{held}\" on this machine. A room's tasks live on one board per \
             machine, so another board would not get them. Connect \"{held}\" again (ai-kanban room connect {room} --board \"{held}\"), {fix}"
        )
        .into());
    }
    if published_before > store.own_tasks_held(&room)? {
        return Err(format!(
            "room {room} holds {published_before} task(s) this machine published from a board it no longer has. A board \
             connected now would not get them back: they read as this machine's own echoes. To start over in the room, \
             give this machine a key of its own (relay admin: key add <label>), set it (ai-kanban remote <url> --key <key>), \
             then connect -- the old tasks then arrive as anyone else's"
        )
        .into());
    }
    let (name, publish) = match &pick {
        Pick::Board(pid) => {
            let board = store.project(*pid)?;
            if let Some(r) = store.room_of(*pid)? {
                return Err(format!("board {} is already connected to room {r}", board.name).into());
            }
            (board.name, Some(store.publish_summary(*pid)?))
        }
        Pick::New(name) => (name.clone(), None),
    };

    let tag = store.origin_tag()?.unwrap_or_default();
    match publish {
        Some(p) => {
            println!("board \"{name}\" -> room \"{room}\" on {}", cfg.url);
            println!("  will publish:  {} tasks, {} events, {} notes   (as #N-{tag})", p.tasks, p.events, p.notes);
        }
        None => println!("a new board \"{name}\" -> room \"{room}\" on {}", cfg.url),
    }
    if let Some(r) = &existing {
        let who = r.members.iter().map(|m| format!("{} ({})", m.user, m.tag)).collect::<Vec<_>>().join(", ");
        println!("  will receive:  {} changes{}", r.changes, if who.is_empty() { String::new() } else { format!(" from {who}") });
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

    let pid = match pick {
        Pick::Board(pid) => pid,
        Pick::New(name) => {
            let p = store.create_board(&name)?;
            println!("board {} created.", p.name);
            give_it_a_folder(store, p.id, &p.name)?;
            p.id
        }
    };
    c.join(&room, Some(&name))?;
    store.connect_board(pid, &room)?;
    match sync_board(store, &c, pid) {
        Ok(s) => println!("connected. Published {} changes, received {}. The board now shows as: Board: {name} @{room}", s.pushed, s.applied),
        Err(e) => println!("connected, but the first sync failed: {e}\nIt is retried on the next sync; nothing is lost."),
    }
    Ok(())
}

/// The room's name for a new board, if no board has it yet. Checked when it is chosen, so the
/// person is not told after confirming.
fn free_name(store: &Store, name: &str) -> Result<String, Box<dyn std::error::Error>> {
    if store.all_projects()?.iter().any(|p| p.name.eq_ignore_ascii_case(name)) {
        return Err(format!(
            "there is already a board called {name}. Connect that one (--board {name}), or make one with another \
             name: ai-kanban board add <name>, then --board <name>"
        )
        .into());
    }
    Ok(name.to_string())
}

/// A board with no folder is one no session opens: an agent in the project resolves to another
/// board, and the room's tasks would land where nobody looks. So the folder this runs in is
/// offered, when it belongs to no board yet -- defaulting to no, since that may be anywhere.
fn give_it_a_folder(store: &Store, pid: i64, name: &str) -> CliResult {
    let here = std::env::current_dir()?;
    let hint = || println!("It has no folder yet. Give it one, so sessions there open it: ai-kanban repo add <path> --board {name}");
    if store.find_project(&here)?.is_some() || !std::io::stdin().is_terminal() {
        hint();
        return Ok(());
    }
    let answer = ask(&format!("Open it from this folder ({})? [y/N]:", here.display()), "")?;
    if matches!(answer.as_str(), "y" | "yes") {
        let repo = store.add_repo(pid, &here, None, crate::core::model::Actor::User)?;
        println!("{} added to board {name}.", repo.name);
    } else {
        hint();
    }
    Ok(())
}

/// The board to connect, asked for when neither `--board` nor `--new-board` was given. Boards
/// already in a room are not offered, the one this folder resolves to comes first, and 0 makes
/// a new one.
fn pick_board(store: &Store, room: &str) -> Result<Pick, Box<dyn std::error::Error>> {
    let connected: Vec<i64> = store.connected_boards()?.into_iter().map(|(p, _)| p).collect();
    let here = std::env::current_dir().ok().and_then(|d| store.find_project(&d).ok().flatten()).map(|p| p.id);
    let mut boards: Vec<_> = store.all_projects()?.into_iter().filter(|p| !connected.contains(&p.id)).collect();
    boards.sort_by_key(|p| (Some(p.id) != here, p.name.to_ascii_lowercase()));
    if !std::io::stdin().is_terminal() {
        let names = boards.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        return Err(if boards.is_empty() {
            "no board on this machine is free to connect: pass --new-board to make one for this room".to_string()
        } else {
            format!("which board? pass --board <name> (one of: {names}), or --new-board to make one for this room")
        }
        .into());
    }
    println!("Connect which local board to room \"{room}\"?");
    println!("  0) a new board");
    for (i, p) in boards.iter().enumerate() {
        let mark = if Some(p.id) == here { "   (this folder)" } else { "" };
        println!("  {}) {}{mark}", i + 1, p.name);
    }
    let default = if boards.is_empty() { 0 } else { 1 };
    let answer = ask(&format!("Board [{default}]:"), "--board <name> or --new-board")?;
    let n: usize = if answer.is_empty() { default } else { answer.parse().map_err(|_| format!("{answer:?} is not one of the numbers"))? };
    if n == 0 {
        let name = ask_as_typed(&format!("Name for the new board [{room}]:"), "--new-board")?;
        return Ok(Pick::New(free_name(store, if name.is_empty() { room } else { &name })?));
    }
    boards.get(n - 1).map(|p| Pick::Board(p.id)).ok_or_else(|| format!("{n} is not one of the numbers").into())
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
