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
    file_path: Option<String>,
    session_id: Option<String>,
}

fn read_input() -> HookInput {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return HookInput::default();
    }
    match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => HookInput {
            cwd: v.get("cwd").and_then(|c| c.as_str()).map(String::from),
            file_path: v.pointer("/tool_input/file_path").and_then(|c| c.as_str()).map(String::from),
            session_id: v.get("session_id").and_then(|c| c.as_str()).map(String::from),
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

// ---------------------------------------------------------------------------
// PostToolUse -- contextual recall
// ---------------------------------------------------------------------------

/// Notes shown for one file before the rest become a count. A file with a dozen notes
/// almost certainly has three that matter; dumping all of them turns a helpful aside into
/// an interruption.
const MAX_NOTES: usize = 3;

/// Runs the `PostToolUse` hook: surfaces what is known about the file just touched.
///
/// # Why this event, and not `FileChanged`
///
/// `FileChanged`'s matcher builds a *literal filename watch list* in the working directory,
/// so it is built for watching specific config files, not for noticing which of a thousand
/// source files an agent just opened. `PostToolUse` matches on tool name and hands over
/// `tool_input.file_path`, which is exactly the question being asked.
///
/// # The constraint that shapes everything here
///
/// This fires on **every** file read and edit. Anything it prints that was not worth
/// printing is a tax on every tool call in the session, and the fastest route to the plugin
/// being disabled -- which would take the board and the MCP server with it. So it stays
/// silent unless a note is genuinely about this file, and it never repeats itself.
pub fn post_tool_use() {
    if let Some(context) = build_file_context() {
        let payload = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": context,
            }
        });
        println!("{payload}");
    }
}

fn build_file_context() -> Option<String> {
    let input = read_input();
    let file = input.file_path.clone()?;
    let store = Store::open_existing().ok()??;
    let project = store.find_project(&start_dir(&input)).ok()??;

    let notes = store.notes_for_path(project.id, &file).ok()?;
    if notes.is_empty() {
        return None;
    }

    // Suppress notes already shown this session. Reading the same file five times must not
    // deliver the same paragraph five times -- that is the behaviour that makes a hook feel
    // like nagging rather than help.
    let seen = SeenNotes::for_session(input.session_id.as_deref());
    let fresh: Vec<_> = notes.iter().filter(|n| !seen.contains(n.id)).collect();
    if fresh.is_empty() {
        return None;
    }
    seen.record(fresh.iter().map(|n| n.id));

    let now = crate::core::now();
    let shown = fresh.iter().take(MAX_NOTES);
    let mut out = format!("Recorded previously about {}:\n", short_path(&file));
    for n in shown {
        out.push_str(&format!("\nnote #{} \"{}\"  ({})\n", n.id, n.title, render::age(now, n.updated_at)));
        if !n.body.is_empty() {
            out.push_str(&format!("  {}\n", n.body.replace('\n', "\n  ")));
        }
    }
    if fresh.len() > MAX_NOTES {
        out.push_str(&format!("\n...and {} more for this file (recall to see them)\n", fresh.len() - MAX_NOTES));
    }
    // Named here because this is the moment the agent can act on it: it is looking at the
    // code the claim is about. Stale memory the agent trusts is worse than no memory, and
    // nothing else in the system decides when a note stops being true.
    out.push_str("\nIf any of this is now wrong, correct it with note_update.");
    Some(out)
}

/// Per-session record of which notes have already been surfaced.
///
/// Deliberately a file in the OS temp directory rather than a table: the hook opens the
/// store read-only, and writing "I mentioned this" into the user's memory would make an
/// observer into a participant. Losing this state costs a repeated note, which is why every
/// failure here is ignored rather than reported.
struct SeenNotes {
    path: Option<PathBuf>,
    ids: Vec<i64>,
}

impl SeenNotes {
    fn for_session(session: Option<&str>) -> Self {
        // No session id means no way to scope the state; degrade to showing the note rather
        // than sharing one file across unrelated sessions.
        let Some(session) = session.filter(|s| !s.is_empty() && is_safe_id(s)) else {
            return Self { path: None, ids: vec![] };
        };
        let path = std::env::temp_dir().join(format!("ai-kanban-seen-{session}"));
        let ids = std::fs::read_to_string(&path)
            .map(|t| t.lines().filter_map(|l| l.trim().parse::<i64>().ok()).collect())
            .unwrap_or_default();
        Self { path: Some(path), ids }
    }

    fn contains(&self, id: i64) -> bool {
        self.ids.contains(&id)
    }

    fn record(&self, ids: impl Iterator<Item = i64>) {
        let Some(path) = &self.path else { return };
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            for id in ids {
                let _ = writeln!(f, "{id}");
            }
        }
    }
}

/// Session ids come from the host, but they are interpolated into a filename, so they are
/// checked rather than trusted.
fn is_safe_id(s: &str) -> bool {
    s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Trims an absolute path to something readable. The agent knows which repo it is in; the
/// leading /Users/someone/code/ is noise repeated on every hit.
fn short_path(p: &str) -> &str {
    p.rsplit_once('/').map(|(_, f)| f).filter(|_| p.len() > 60).unwrap_or(p)
}
