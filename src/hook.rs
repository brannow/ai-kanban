//! The hook adapter: context the agent never had to ask for.
//!
//! # Why this exists at all
//!
//! Every other part of this project makes the board *possible* to use. None of it makes an
//! agent actually use one. The stated failure this project is built against is that agents
//! skip the board -- and a better tool surface does not fix that, because calling a tool is
//! still a choice made under time pressure by something optimising for the shortest path to
//! the answer.
//!
//! A `SessionStart` hook is the one mechanism that does not depend on that choice. The
//! board is in context before the first prompt; the agent does not decide to read it.
//!
//! # Why this speaks the hook protocol directly instead of printing text
//!
//! `SessionStart` fires before Claude Code has finished connecting to MCP servers, so a
//! hook on this event cannot call our own MCP tools -- it has to be a command. Emitting the
//! hook's JSON from the binary means the plugin needs no wrapper script, no `jq`, and works
//! on Windows.
//!
//! # The rules this adapter must not break
//!
//! **Never create anything.** This runs in every directory the user opens Claude Code in.
//! Creating a board here would mint one for every scratch folder and tarball they visit.
//!
//! **Never fail loudly.** A hook that prints an error on every session start gets deleted,
//! and it takes the bundled MCP server with it. Every failure path here exits 0 in silence.

use crate::core::model::BoardQuery;
use crate::core::Store;
use crate::render;
use std::io::Read;
use std::path::PathBuf;

/// Claude Code caps hook output at 10,000 characters. Staying under it deliberately rather
/// than being truncated at the boundary, which would cut a board mid-row.
const MAX_CONTEXT: usize = 9_000;

/// What the board is for, addressed to the agent reading it.
///
/// This is a **product artifact**, not developer documentation: its audience is an agent
/// working in someone else's repository, and it is the payload that has to cause adoption.
/// It is deliberately kept apart from this repo's own `CLAUDE.md`, which addresses a
/// contributor working on ai-kanban itself. Different audiences, different lifecycles;
/// merging them is how both end up wrong.
const GUIDANCE: &str = "\
This board is your memory for this project. It persists across sessions, so anything you \
record here is still there for whoever picks the work up next -- including you, cold, in a \
month.

Worth doing without being asked:
  - Spotted a bug, a TODO or a side quest while doing something else? File it (task_add) \
instead of carrying it in your head or dropping it.
  - Moving a task? Say why (task_update with `log`). What changed is recoverable from the \
diff; why it changed is not.
  - Worked something out about this codebase that was not obvious? Record it (note_add), \
with the files it concerns. That is knowledge that would otherwise die with this session.
  - About to debug something that feels familiar? Check recall first -- you may have \
already solved it here or on another project.";

#[derive(Debug, Default)]
struct HookInput {
    cwd: Option<String>,
}

fn read_input() -> HookInput {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return HookInput::default();
    }
    match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => HookInput {
            cwd: v.get("cwd").and_then(|c| c.as_str()).map(String::from),
        },
        Err(_) => HookInput::default(),
    }
}

/// `CLAUDE_PROJECT_DIR` first for consistency with the MCP server, then the `cwd` the hook
/// was handed, then the process working directory.
fn start_dir(input: &HookInput) -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_PROJECT_DIR") {
        return PathBuf::from(d);
    }
    if let Some(c) = &input.cwd {
        return PathBuf::from(c);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Runs the `SessionStart` hook. Always exits 0; prints nothing when it has nothing to say.
pub fn session_start() {
    if let Some(context) = build_context() {
        let payload = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }
        });
        println!("{payload}");
    }
}

/// Every failure is `None`, never an error. A missing store means the user has not used
/// ai-kanban yet, which is normal and not worth a word.
fn build_context() -> Option<String> {
    let store = Store::open_existing().ok()??;
    let input = read_input();
    context_for(&store, &start_dir(&input))
}

/// The part worth testing, separated from the environment it normally reads.
pub fn context_for(store: &Store, start: &std::path::Path) -> Option<String> {
    // Lookup, never resolve: this runs in every directory the user opens Claude Code in.
    //
    // No board means nothing to say. A "you could start a board here" nudge was tried and
    // dropped: the store exists as soon as ai-kanban is used once, so from then on that line
    // would appear in every directory forever -- $HOME, /tmp, an unpacked tarball, someone
    // else's clone. Chattiness is what gets a hook uninstalled, and the agent can already
    // see the tools exist without being told.
    let project = store.find_project(start).ok()??;

    let snap = store.board(project.id, &BoardQuery::board()).ok()?;
    let board = render::board(&snap);

    let mut out = String::with_capacity(board.len() + GUIDANCE.len() + 2);
    out.push_str(&board);
    out.push('\n');
    out.push_str(GUIDANCE);

    if out.len() > MAX_CONTEXT {
        // Trim the board rather than the guidance: a partial board is still useful, whereas
        // guidance cut in half reads as a broken instruction.
        out.truncate(MAX_CONTEXT);
        out.push_str("\n...(truncated)\n");
    }
    Some(out)
}
