//! What `room list`, `show`, `connect` and `disconnect` do, as data. Two front ends drive it:
//! the CLI, which asks and prints around each step, and the MCP tools, through which the agent
//! manages rooms itself (#1119). One implementation, so the refusals -- each one a bug found the
//! hard way -- cannot drift apart between them.
//!
//! The store comes behind its mutex and is locked only for local steps. Pulling a room's whole
//! log takes seconds from across the world, and in the MCP process every other tool call would
//! queue behind it.

use super::client::{Client, RemoteError};
use super::engine::check_identity;
use crate::core::model::{BoardQuery, BoardSnapshot};
use crate::core::project::FolderBoard;
use crate::core::sync::{PublishSummary, SyncStatus};
use crate::core::Store;
use crate::sync::protocol::{RoomInfo, MAX_PAGE};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn lock(s: &Mutex<Store>) -> MutexGuard<'_, Store> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn parse_room(room: &str) -> Result<String> {
    crate::sync::protocol::room_name(room)
        .ok_or_else(|| format!("{room:?} is not a room name: lowercase letters, digits, '-' and '_'").into())
}

pub fn find_room(c: &Client, room: &str) -> std::result::Result<Option<RoomInfo>, RemoteError> {
    Ok(c.rooms()?.into_iter().find(|r| r.name == room))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One room on the relay, and the local board connected to it with how its sync is doing.
pub struct Listed {
    pub info: RoomInfo,
    pub mine: Option<(String, SyncStatus)>,
}

pub enum Listing {
    Rooms(Vec<Listed>),
    /// The relay did not answer (or sync is off): what this machine knows of its own shared
    /// boards, as (room, board, status).
    Unreachable { error: String, mine: Vec<(String, String, SyncStatus)> },
}

/// Every room on the relay, and for the ones a board here shares, how its sync is doing. One
/// answer for both: what is out there and how mine are doing are the same question. `None`
/// for the client when this run may not use the network.
pub fn list(store: &Mutex<Store>, c: Option<&Client>) -> Result<Listing> {
    let rooms = match c {
        Some(c) => {
            super::engine::claim(&lock(store), c);
            c.rooms().map_err(|e| e.to_string())
        }
        None => Err("sync is off for this run".to_string()),
    };
    let s = lock(store);
    match rooms {
        Err(error) => {
            let mut mine = Vec::new();
            for (pid, room) in s.connected_boards()? {
                mine.push((room, s.project(pid)?.name, s.sync_status(pid)?.unwrap_or_default()));
            }
            Ok(Listing::Unreachable { error, mine })
        }
        Ok(rooms) => {
            let mut out = Vec::new();
            for info in rooms {
                let mine = match s.board_of_room(&info.name)? {
                    Some(pid) => Some((s.project(pid)?.name, s.sync_status(pid)?.unwrap_or_default())),
                    None => None,
                };
                out.push(Listed { info, mine });
            }
            Ok(Listing::Rooms(out))
        }
    }
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

/// A room's content as a board, pulled into a throwaway store. `published_here` counts the
/// tasks in it created under one of this store's tags: what a board of this machine published.
pub struct Preview {
    pub info: RoomInfo,
    /// `None` for a room with nothing in it.
    pub board: Option<BoardSnapshot>,
    pub published_here: i64,
}

/// The tag a preview store publishes under. The relay never issues it -- tags it hands out are
/// an initial and a number -- so no change in a room can look like the preview's own.
const PREVIEW_TAG: &str = "preview";

/// What a room holds, without connecting to it: the room's log pulled into a throwaway
/// in-memory store and read as the board view reads any board. The relay interprets nothing;
/// the counts and claims are the client's own board logic.
///
/// Pull and apply only. `sync_board` would also push, sending the temporary board's own
/// creation into the room; `check_identity` would give the store this machine's tag, and this
/// machine's published tasks would then read as the preview's own.
pub fn look(c: &Client, info: &RoomInfo, own_tags: &[String]) -> Result<Preview> {
    if info.changes == 0 {
        return Ok(Preview { info: info.clone(), board: None, published_here: 0 });
    }
    let mut mine = std::collections::HashSet::new();
    let (tmp, pid) = Store::preview(&info.name, info.board.as_deref().unwrap_or(&info.name), PREVIEW_TAG)?;
    let mut since = 0;
    loop {
        let page = c.pull(&info.name, since, MAX_PAGE).map_err(|e| match e {
            // A relay from before reading was open to every key.
            RemoteError::NotFound(m) if m.contains("not a member") => format!(
                "the relay at {} shows a room only to its members -- it is older than this ai-kanban. Update the relay",
                c.url()
            ),
            e => e.to_string(),
        })?;
        let Some(last) = page.changes.iter().map(|p| p.seq).max() else { break };
        since = last;
        for p in &page.changes {
            if p.change.entity.kind == "task" && own_tags.contains(&p.change.entity.origin) {
                mine.insert((p.change.entity.origin.clone(), p.change.entity.id));
            }
        }
        tmp.apply(pid, &page.changes)?;
        if !page.more {
            break;
        }
    }
    let members: Vec<(String, String)> = info.members.iter().map(|m| (m.tag.clone(), m.user.clone())).collect();
    tmp.sync_succeeded(pid, &members)?;
    let board = tmp.board(pid, &BoardQuery::board())?;
    Ok(Preview { info: info.clone(), board: Some(board), published_here: mine.len() as i64 })
}

/// `look` for a room by name, with the local board connected to it if there is one. `None`
/// when the relay has no such room.
pub fn show(store: &Mutex<Store>, c: &Client, room: &str) -> Result<Option<(Preview, Option<String>)>> {
    let room = parse_room(room)?;
    let own = {
        let s = lock(store);
        super::engine::claim(&s, c);
        s.own_tags()?
    };
    let Some(info) = find_room(c, &room)? else { return Ok(None) };
    let preview = look(c, &info, &own)?;
    let s = lock(store);
    let here = match s.board_of_room(&room)? {
        Some(pid) => Some(s.project(pid)?.name),
        None => None,
    };
    Ok(Some((preview, here)))
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

/// What is known about a room before anyone chooses a board for it.
pub struct Inspection {
    pub room: String,
    /// `None`: the room does not exist yet, and connecting creates it.
    pub existing: Option<RoomInfo>,
    /// What is in it, or why that could not be shown. `None` when it does not exist.
    pub preview: Option<std::result::Result<Preview, String>>,
    /// Whether the room is known to hold nothing this machine published: it does not exist yet,
    /// or its log has none of this store's tasks. Then what this store remembers sending to a
    /// room by that name is of one that is gone (deleted and reopened, or a relay set up anew),
    /// and is forgotten once the connect is confirmed.
    fresh: bool,
    published_before: i64,
}

/// Checks this machine may sync with the relay, that the room is not connected here already,
/// and looks into the room. Writes nothing (except what `check_identity` records).
pub fn inspect(store: &Mutex<Store>, c: &Client, room: &str) -> Result<Inspection> {
    let room = parse_room(room)?;
    let own = {
        let s = lock(store);
        check_identity(&s, c)?;
        if let Some(pid) = s.board_of_room(&room)? {
            return Err(format!("room {room} is already connected to board {} on this machine", s.project(pid)?.name).into());
        }
        s.own_tags()?
    };
    let existing = find_room(c, &room)?;
    let mut published_before = 0;
    let mut fresh = existing.is_none();
    // A relay from before open reads cannot show it; connecting must still work there.
    let preview = existing.as_ref().map(|info| {
        let p = look(c, info, &own).map_err(|e| e.to_string());
        if let Ok(p) = &p {
            published_before = p.published_here;
            fresh = p.published_here == 0;
        }
        p
    });
    Ok(Inspection { room, existing, preview, fresh, published_before })
}

/// The board a room connects to.
#[derive(Debug, Clone)]
pub enum Choice {
    /// One that exists.
    Board(i64),
    /// A new one by this name, with no folder (`--new-board`, the picker's 0).
    Named(String),
    /// The one a session in this folder would create: keyed by the folder's git remote, as
    /// every teammate's clone of the repo is, and opened from this folder.
    Folder(PathBuf),
}

/// The board a session in `dir` opens, existing or to be created -- the agent's default.
pub fn choose_folder(store: &Mutex<Store>, dir: &Path) -> Result<Choice> {
    Ok(match lock(store).folder_board(dir)? {
        FolderBoard::Existing(p) => Choice::Board(p.id),
        FolderBoard::New(_) => Choice::Folder(dir.to_path_buf()),
    })
}

/// A connect, checked and ready, waiting only for the yes.
pub struct ConnectPlan {
    pub inspection: Inspection,
    pub choice: Choice,
    pub board_name: String,
    /// What connecting publishes. `None` for a board that does not exist yet.
    pub publish: Option<PublishSummary>,
}

impl ConnectPlan {
    /// Whether connecting would publish anything that is already on the board. Only then is it
    /// a decision: a new or empty board sends nothing but what is written from now on.
    pub fn publishes_history(&self) -> bool {
        self.publish.is_some_and(|p| p.tasks + p.notes + p.events > 0)
    }
}

/// Every refusal, before anything is written: the backup, the join, a new board.
pub fn plan(store: &Mutex<Store>, inspection: Inspection, choice: Choice) -> Result<ConnectPlan> {
    let s = lock(store);
    let room = &inspection.room;
    let target = match &choice {
        Choice::Board(pid) => Some(*pid),
        _ => None,
    };
    // Skipped only for a room that does not exist: one that does may hold tasks this machine
    // received onto another board, and a room of nothing but those still looks "fresh".
    if let Some((elsewhere, tasks)) = if inspection.existing.is_none() { None } else { s.room_held_elsewhere(room, target)? } {
        let held = s.project(elsewhere)?.name;
        let fix = match &choice {
            Choice::Board(pid) => format!("or merge it into this one first: ai-kanban board merge \"{}\" \"{held}\"", s.project(*pid)?.name),
            _ => "or merge it into the board you want first (ai-kanban board merge <board> <that board>)".to_string(),
        };
        return Err(format!(
            "{tasks} task(s) of room {room} are on board \"{held}\" on this machine. A room's tasks live on one board per \
             machine, so another board would not get them. Connect \"{held}\" again (ai-kanban room connect {room} --board \"{held}\"), {fix}"
        )
        .into());
    }
    if inspection.published_before > s.own_tasks_held(room)? {
        return Err(format!(
            "room {room} holds {} task(s) this machine published from a board it no longer has. A board \
             connected now would not get them back: they read as this machine's own echoes. To start over in the room, \
             give this machine a key of its own (relay admin: key add <label>), set it (ai-kanban remote <url> --key <key>), \
             then connect -- the old tasks then arrive as anyone else's",
            inspection.published_before
        )
        .into());
    }
    let (board_name, publish) = match &choice {
        Choice::Board(pid) => {
            let board = s.project(*pid)?;
            if let Some(r) = s.room_of(*pid)? {
                return Err(format!("board {} is already connected to room {r}", board.name).into());
            }
            (board.name, Some(s.publish_summary(*pid)?))
        }
        Choice::Named(name) => (free_name(&s, name)?, None),
        Choice::Folder(dir) => match s.folder_board(dir)? {
            FolderBoard::New(name) => (name, None),
            // Created since `choose_folder`, by a session in the same folder.
            FolderBoard::Existing(p) => return Err(format!("this folder has a board now ({}): connect again", p.name).into()),
        },
    };
    drop(s);
    Ok(ConnectPlan { inspection, choice, board_name, publish })
}

/// The room's name for a new board, if no board has it yet. Checked before confirming, so the
/// person is not told afterwards.
fn free_name(s: &Store, name: &str) -> Result<String> {
    if s.all_projects()?.iter().any(|p| p.name.eq_ignore_ascii_case(name)) {
        return Err(format!(
            "there is already a board called {name}. Connect that one (--board {name}), or make one with another \
             name: ai-kanban board add <name>, then --board <name>"
        )
        .into());
    }
    Ok(name.to_string())
}

/// What a connect did. The first sync is the caller's: the CLI runs it to the end, a tool call
/// within a budget.
pub struct Connected {
    pub pid: i64,
    pub board_name: String,
    /// Whether the board was made for this room.
    pub created: bool,
    pub backup: PathBuf,
}

/// Carries out a confirmed plan: a backup before anything leaves the machine, then the join,
/// then the board (made now if it is new) is connected.
pub fn connect(store: &Mutex<Store>, c: &Client, plan: ConnectPlan) -> Result<Connected> {
    let room = &plan.inspection.room;
    let backup = {
        let s = lock(store);
        // Whole seconds name it, so a second connect within the same one gets a counter rather
        // than a refusal to overwrite the first backup.
        let dir = Store::default_path()?.parent().map(|d| d.to_path_buf()).unwrap_or_default();
        let stamp = crate::core::now();
        let mut backup = dir.join(format!("kanban-before-connect-{room}-{stamp}.db"));
        for n in 2.. {
            if !backup.exists() {
                break;
            }
            backup = dir.join(format!("kanban-before-connect-{room}-{stamp}-{n}.db"));
        }
        s.backup_to(&backup)?;
        backup
    };
    // Joined before a new board is made: a relay that fails here then leaves nothing behind
    // locally, and a join without a board is only a member that has not written yet.
    let joined = c.join(room, Some(&plan.board_name))?;
    let s = lock(store);
    let (pid, created) = match &plan.choice {
        Choice::Board(pid) => (*pid, false),
        Choice::Named(name) => (s.create_board(name)?.id, true),
        Choice::Folder(dir) => {
            let r = s.resolve_project(dir)?;
            (r.project.id, r.created)
        }
    };
    s.connect_board(pid, room)?;
    if let Some(id) = joined.room.id {
        s.set_relay_room(pid, id)?;
    }
    // A room that holds none of this machine's work gets what the board published before, with
    // the clocks it was written at (`Store::republish_into_fresh_room`). Others' work is carried
    // only by a relay that takes carried changes, which is one that names its instance.
    if plan.inspection.fresh {
        drop(s);
        let carry = c.hello().map(|h| h.instance.is_some()).unwrap_or(false);
        let s = lock(store);
        s.republish_into_fresh_room(pid, carry)?;
        let board_name = s.project(pid)?.name;
        return Ok(Connected { pid, board_name, created, backup });
    }
    let board_name = s.project(pid)?.name;
    Ok(Connected { pid, board_name, created, backup })
}

// ---------------------------------------------------------------------------
// disconnect
// ---------------------------------------------------------------------------

/// Uncouples a board from its room, telling the relay when it can. Returns why the relay could
/// not be told, if it could not: best effort, since the relay only uses membership for its own
/// housekeeping, and offline the board is uncoupled anyway. Unsent changes are the caller's to
/// settle first.
pub fn uncouple(store: &Mutex<Store>, c: Option<&Client>, pid: i64, room: &str, wipe_foreign: bool) -> Result<Option<String>> {
    let told = match c {
        Some(c) => c.leave(room).err().map(|e| e.to_string()),
        None => None,
    };
    lock(store).disconnect_board(pid, wipe_foreign)?;
    Ok(told)
}
