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
already solved it here or on another project.
  - Told what you are working on (\"we're doing the contact form now\")? Pass it to board \
as `workstream`. The board narrows to that work and tasks you file join it, so the next \
session starts on the right slice instead of the whole project.

These are ai-kanban's MCP tools; your tool list shows them under a longer namespaced name.";

#[derive(Debug, Default)]
struct HookInput {
    cwd: Option<String>,
    file_path: Option<String>,
    /// Bash's `tool_input` carries this instead of `file_path`. See `paths_in_command`.
    command: Option<String>,
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
            command: v.pointer("/tool_input/command").and_then(|c| c.as_str()).map(String::from),
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

    // Honour the board's current workstream. This is the reason the workstream is sticky
    // state rather than an argument: THIS call takes no arguments and never can. It is the
    // cold-start view -- the board an agent reads before the first prompt -- so a scope the
    // caller has to pass would leave exactly the view that motivated the feature unscoped.
    //
    // `unwrap_or(None)` rather than `?`: a store this binary has not migrated yet has no
    // workstream tables, and that must degrade to an unscoped board rather than to no board
    // at all. Hook code never fails loudly.
    let workstream = store.current_workstream(project.id).unwrap_or(None);
    let q = BoardQuery::board().with_workstream(workstream.map(|w| w.id));
    let snap = store.board(project.id, &q).ok()?;
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

/// How many distinct files one shell command is examined for, and how many of its tokens are
/// considered. Both bound work that happens on **every** `Bash` call in a session; a
/// generated command or a heredoc can be arbitrarily long, and no useful command names five
/// files worth reporting on at once.
const MAX_COMMAND_PATHS: usize = 4;
const MAX_COMMAND_TOKENS: usize = 40;

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
    // Cheapest possible bail, before opening anything: this fires on every matched tool call.
    if input.file_path.is_none() && input.command.is_none() {
        return None;
    }
    let store = Store::open_existing().ok()??;
    let start = start_dir(&input);
    let files = files_touched(&input, &start);
    file_context(&store, &start, &files, input.session_id.as_deref())
}

/// The part worth testing, separated from stdin and the environment.
///
/// `session` scopes the already-shown record; `None` disables it, which is what tests want.
pub fn file_context(
    store: &Store, start: &std::path::Path, files: &[String], session: Option<&str>,
) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    // No board here means nothing to say, whatever the command touched.
    let project = store.find_project(start).ok()??;

    // Normalise both sides here rather than trusting the caller, because the two arrive in
    // different forms: the host hands `Read` the path as the agent wrote it, while extraction
    // from a shell command canonicalises in order to test existence. On macOS that difference
    // is visible -- `/var` resolves to `/private/var` -- and an uncanonicalised root then
    // fails to strip from a canonicalised file, so the header prints a full absolute path
    // where it should print a repo-relative one.
    let root = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let files: Vec<String> = files.iter().map(|f| canonical_str(f)).collect();
    let files = &files;

    // Kept grouped by file rather than merged into one list: a note is a claim about a
    // specific file, and which file is most of what makes it actionable.
    let mut groups: Vec<(&String, Vec<crate::core::model::Note>)> = Vec::new();
    for f in files {
        let Ok(notes) = store.notes_for_path(project.id, f) else { continue };
        if !notes.is_empty() {
            groups.push((f, notes));
        }
    }
    if groups.is_empty() {
        return None;
    }

    // Suppress notes already shown this session. Reading the same file five times must not
    // deliver the same paragraph five times -- that is the behaviour that makes a hook feel
    // like nagging rather than help.
    let seen = SeenNotes::for_session(session);

    let now = crate::core::now();
    let mut out = String::new();
    let mut shown = 0usize;
    let mut withheld = 0usize;
    let mut recorded: Vec<i64> = Vec::new();

    for (file, notes) in &groups {
        let fresh: Vec<_> = notes.iter().filter(|n| !seen.contains(n.id)).collect();
        if fresh.is_empty() {
            continue;
        }
        // The cap is on the whole message, not per file. One shell command touching four
        // documented files must not cost four times as much as reading one.
        let room = MAX_NOTES.saturating_sub(shown);
        if room == 0 {
            withheld += fresh.len();
            continue;
        }

        out.push_str(&format!("Recorded previously about {}:\n", short_path(file, &root)));
        for n in fresh.iter().take(room) {
            out.push_str(&format!("\nnote #{} \"{}\"  ({})\n", n.id, n.title, render::age(now, n.updated_at)));
            if !n.body.is_empty() {
                out.push_str(&format!("  {}\n", n.body.replace('\n', "\n  ")));
            }
            shown += 1;
        }
        withheld += fresh.len().saturating_sub(room);
        // Everything fresh is marked seen, including what did not fit. Showing it on the
        // next tool call would be the same interruption, one step later.
        recorded.extend(fresh.iter().map(|n| n.id));
    }

    if shown == 0 {
        return None;
    }
    seen.record(recorded.into_iter());

    if withheld > 0 {
        out.push_str(&format!("\n...and {withheld} more (recall to see them)\n"));
    }
    // Named here because this is the moment the agent can act on it: it is looking at the
    // code the claim is about. Stale memory the agent trusts is worse than no memory, and
    // nothing else in the system decides when a note stops being true.
    out.push_str("\nIf any of this is now wrong, correct it with note_update.");
    Some(out)
}

/// Files a shell command appears to touch.
///
/// # Why this exists
///
/// `PostToolUse` hands `Read`, `Edit` and `Write` a `tool_input.file_path`. It hands `Bash` a
/// `command` string and nothing else. So a matcher of `Read|Edit|Write` misses every file an
/// agent opens with `cat`, `sed`, `head` or `grep` -- and that is not a corner case: this
/// project's own `CLAUDE.md` tells agents to read that way, and Claude Code's auto mode does
/// too. Half of contextual recall was silently not firing, which is worse than it not
/// existing, because nothing indicates the memory is being skipped.
///
/// # Why the parsing is deliberately dumb
///
/// Split on whitespace and shell separators, drop flags and punctuation, keep whatever
/// resolves to a real file. There is no attempt to understand redirects, quoting, expansion
/// or which argument of which command is an input.
///
/// That is not laziness, it is where the filtering actually happens: **the existence check is
/// the only filter that matters.** A token that is not a file on disk is discarded, so the
/// cost of a sloppy candidate is one `canonicalize` call. Parsing shell properly would be a
/// large amount of code to slightly reduce the number of stat calls, and would still be wrong
/// on the first command that used a construct nobody thought of.
///
/// The false positive it does admit -- a path named in a command that never read it, like
/// `rm old.rs` -- surfaces what is known about a file the agent is acting on. That is not the
/// wrong moment to say it.
pub fn paths_in_command(command: &str, cwd: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for (checked, raw) in command
        .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>' | '(' | ')'))
        .filter(|t| !t.is_empty())
        .enumerate()
    {
        // Bounded twice: a heredoc or a generated command can be enormous, and this runs on
        // every shell call in the session.
        if out.len() >= MAX_COMMAND_PATHS || checked >= MAX_COMMAND_TOKENS {
            break;
        }
        let tok = raw.trim_matches(|c| matches!(c, '"' | '\'' | ',' | ':'));
        // Flags, and anything long enough to be content rather than a path.
        if tok.is_empty() || tok.starts_with('-') || tok.len() > 256 {
            continue;
        }
        // Cheap narrowing before touching the filesystem. Every path has one or the other;
        // most shell noise (`-n`, `HEAD~1`, `install`) has neither.
        if !tok.contains('/') && !tok.contains('.') {
            continue;
        }

        let joined = if std::path::Path::new(tok).is_absolute() {
            PathBuf::from(tok)
        } else {
            cwd.join(tok)
        };
        // `canonicalize` proves existence and normalises `./` and `..` in one syscall. The
        // stored note paths are matched as suffixes, so resolving symlinks here is harmless.
        let Ok(abs) = std::fs::canonicalize(&joined) else { continue };
        if !abs.is_file() {
            continue;
        }
        let abs = abs.to_string_lossy().into_owned();
        if !out.contains(&abs) {
            out.push(abs);
        }
    }
    out
}

/// The files this tool call touched, however the host chose to describe them.
///
fn files_touched(input: &HookInput, cwd: &std::path::Path) -> Vec<String> {
    if let Some(f) = &input.file_path {
        return vec![f.clone()];
    }
    match &input.command {
        Some(c) => paths_in_command(c, cwd),
        None => Vec::new(),
    }
}

/// Canonical form, or the input unchanged when it cannot be resolved. Falling back rather
/// than dropping: a path that does not resolve may still match a note by suffix, and this
/// adapter's job is to say something useful, not to be strict.
fn canonical_str(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
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

/// Trims an absolute path to something readable: relative to the project when it is inside
/// it, otherwise unchanged.
///
/// Relative rather than a bare basename, because `note.rs` is ambiguous in any repo with
/// more than one, and relative rather than a length threshold, because how noisy a prefix is
/// has nothing to do with how many characters it happens to be.
fn short_path<'a>(path: &'a str, root: &std::path::Path) -> &'a str {
    let root = root.to_string_lossy();
    path.strip_prefix(root.as_ref())
        .map(|r| r.trim_start_matches('/'))
        .filter(|r| !r.is_empty())
        .unwrap_or(path)
}
