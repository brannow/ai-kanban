//! Prose rendering for agents. Shared by every adapter that talks to a model: the MCP
//! tools an agent calls, and the hook that injects context it never asked for.
//!
//! This lives outside core, never in it, because the human's access to this data is
//! meant to be an API: text in core would force a web UI to parse sentences back into
//! objects it already had.
//!
//! What the format is trying to achieve, in order:
//!
//!  1. **State the board first, always.** The agent must never have to guess which project
//!     it is looking at -- that guess is how work gets filed onto the wrong board.
//!  2. **Be scannable, not pretty.** Fixed-width columns and one line per task, because the
//!     reader is a model reading tokens, not a person reading a dashboard.
//!  3. **Cost tokens in proportion to value.** Every character here is paid for on every
//!     call. An expensive board is one an agent stops calling.

use crate::core::event::status_transition;
use crate::core::model::*;

/// Relative age. Absolute timestamps would be noise: what matters is "is this stale",
/// which is a question about distance from now, not about a date.
pub fn ago(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        86400..=2591999 => format!("{}d", d / 86400),
        2592000..=31535999 => format!("{}mo", d / 2592000),
        _ => format!("{}y", d / 31536000),
    }
}

/// The Board Snapshot -- the standard response shape most tools return.
pub fn board(snap: &BoardSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&header(snap));

    if snap.tasks.is_empty() && snap.counts.is_empty() {
        out.push_str("\nNo tasks yet. Anything filed here is remembered across sessions.\n");
        return out;
    }

    // Group by status, preserving the ordering core already applied.
    let mut current: Option<Status> = None;
    for t in &snap.tasks {
        if current != Some(t.status) {
            out.push_str(&format!("\n{}\n", t.status));
            current = Some(t.status);
        }
        out.push_str(&task_line(t, snap.highlight == Some(t.id)));
    }

    // What the cap left out. Stated, never silently dropped -- a board that hides work is
    // worse than one that admits it is showing a slice.
    if !snap.omitted.is_empty() {
        let more = snap.omitted.iter()
            .map(|(s, n)| format!("+{n} {s}"))
            .collect::<Vec<_>>().join(", ");
        out.push_str(&format!("\n  ...{more}\n"));
    }

    if !snap.recent.is_empty() {
        out.push_str("\nrecent\n");
        for e in &snap.recent {
            out.push_str(&event_line(e, snap.now));
        }
    }
    out
}

fn header(snap: &BoardSnapshot) -> String {
    let open = snap.open_count();
    let doing = snap.count_of(Status::Doing);
    let mut bits = vec![format!("{open} open")];
    if doing > 0 { bits.push(format!("{doing} doing")); }
    let done = snap.count_of(Status::Done);
    if done > 0 { bits.push(format!("{done} done")); }
    format!("Board: {}  ({})\n", snap.project.name, bits.join(", "))
}

/// One task, one line. The trailing parenthetical carries only what is *not* obvious from
/// the columns -- origin is always shown because it is the instrumentation for whether the
/// agent files work unprompted, and that question needs to stay visible.
fn task_line(t: &Task, highlighted: bool) -> String {
    let mark = if highlighted { "*" } else { " " };
    let mut meta = vec![t.origin.to_string()];
    if t.task_type != TaskType::Task { meta.insert(0, t.task_type.to_string()); }
    if t.priority != Priority::Normal { meta.push(t.priority.to_string()); }
    if let Some(b) = t.blocked_by { meta.push(format!("blocked by #{b}")); }
    format!("{mark} #{:<4} {:<40} ({})\n", t.id, truncate(&t.title, 40), meta.join(", "))
}

/// Events render by *kind*, because a status change and a recorded decision are different
/// news. A uniform "something happened" line would make the section skippable, and a
/// section nobody reads is a log nobody consumes.
fn event_line(e: &Event, now: i64) -> String {
    let when = ago(now, e.ts);
    let what = match (status_transition(&e.kind), e.task_id) {
        (Some(to), Some(id)) => format!("#{id} -> {to}{}", reason(&e.body)),
        (Some(to), None) => format!("-> {to}"),
        (None, _) => match e.kind.as_str() {
            "created"      => format!("#{} filed: {}", e.task_id.unwrap_or(0), truncate(&e.body, 50)),
            "note_added"   => format!("note \"{}\"", truncate(&e.body, 50)),
            "note_updated" => format!("note revised: {}", truncate(&e.body, 60)),
            "log"          => truncate(&e.body, 70),
            _ => match e.task_id {
                Some(id) => format!("#{id} {}", truncate(&e.body, 55)),
                None => truncate(&e.body, 70),
            },
        },
    };
    format!("  {:<4} {what}\n", when)
}

fn reason(body: &str) -> String {
    if body.is_empty() { String::new() } else { format!(": {}", truncate(body, 55)) }
}

/// One task, everything needed to resume it cold.
pub fn task_detail(d: &TaskDetail) -> String {
    let t = &d.task;
    let mut out = format!("Board: {}\n\n#{} {}\n", d.project.name, t.id, t.title);
    let mut meta = vec![t.status.to_string(), t.task_type.to_string(), format!("{} priority", t.priority), t.origin.to_string()];
    meta.push(format!("filed {} ago", ago(d.now, t.created_at)));
    out.push_str(&format!("  {}\n", meta.join(", ")));

    if !t.body.is_empty() {
        out.push_str(&format!("\n{}\n", t.body));
    }
    if let Some(b) = &d.blocker {
        out.push_str(&format!("\nblocked by #{} {} ({})\n", b.id, b.title, b.status));
    } else if t.status == Status::Blocked {
        // status is authoritative; blocked_by is only annotation. Say so rather than
        // leaving the reader to wonder whether something is missing.
        out.push_str("\nblocked, with no blocking task on this board\n");
    }
    if !d.blocking.is_empty() {
        out.push_str("\nblocking\n");
        for b in &d.blocking {
            out.push_str(&format!("  #{} {} ({})\n", b.id, truncate(&b.title, 45), b.status));
        }
    }
    if !d.notes.is_empty() {
        out.push_str("\nnotes\n");
        for n in &d.notes {
            out.push_str(&format!("  #{} {}  ({} old)\n", n.id, truncate(&n.title, 45), ago(d.now, n.updated_at)));
        }
    }
    if !d.events.is_empty() {
        out.push_str("\nhistory\n");
        for e in &d.events {
            out.push_str(&event_line(e, d.now));
        }
    }
    out
}

/// Recall. Each hit states its kind, its age and its project, and carries a snippet --
/// a list of titles is a search result; a list of snippets is an answer.
pub fn recall(r: &RecallResult, cross_project: bool) -> String {
    if r.hits.is_empty() {
        return empty_recall(r, cross_project);
    }
    let mut out = format!("{} hit{} for \"{}\" in {}\n",
        r.hits.len(), if r.hits.len() == 1 { "" } else { "s" }, r.query, r.scope);

    for h in &r.hits {
        let kind = match h.kind { HitKind::Note => "note ", HitKind::Task => "task ", HitKind::Event => "event" };
        let mut tail = vec![ago(r.now, h.ts)];
        if let Some(s) = h.status { tail.insert(0, s.to_string()); }
        // The project is on every line, not just the header: without it a cross-project
        // result is speculative in exactly the way the board header exists to prevent.
        if cross_project { tail.insert(0, h.project.clone()); }

        let id = match (h.kind, h.task_id) {
            (HitKind::Event, Some(t)) => format!("#{t}"),
            (HitKind::Event, None) => "  --".to_string(),
            _ => format!("#{}", h.id),
        };
        out.push_str(&format!("\n{kind} {:<5} {}  ({})\n", id, truncate(&h.title, 46), tail.join(", ")));
        if !h.snippet.is_empty() {
            out.push_str(&format!("       {}\n", h.snippet.replace('\n', " ")));
        }
    }

    if !cross_project {
        out.push_str("\nSearched this project only. Pass project: \"all\" to search every board.\n");
    }
    out
}

/// "No hits" alone causes a follow-up call. Saying what the store *does* hold, and that a
/// wider search exists, turns a dead end into a next step.
fn empty_recall(r: &RecallResult, cross_project: bool) -> String {
    let a = &r.available;
    let mut out = format!("No hits for \"{}\" in {}.\n", r.query, r.scope);
    if a.notes == 0 && a.tasks == 0 && a.events == 0 {
        out.push_str("\nThis board is empty -- nothing has been recorded here yet.\n");
        return out;
    }
    out.push_str(&format!("\nThis board holds {} note{}, {} task{} and {} event{}.\n",
        a.notes, plural(a.notes), a.tasks, plural(a.tasks), a.events, plural(a.events)));
    if !cross_project && a.projects > 1 {
        out.push_str(&format!("Pass project: \"all\" to search the other {} board{}.\n",
            a.projects - 1, plural(a.projects - 1)));
    }
    out
}

/// `board(project: "all")` -- summaries, never a merged task list. Every open task across
/// every project is not a board, it is a pile.
pub fn project_summaries(sums: &[ProjectSummary], total: usize, now: i64) -> String {
    if sums.is_empty() {
        return "No boards yet.\n".to_string();
    }
    let mut out = format!("{} board{}\n", total, plural(total));
    for s in sums {
        let when = s.last_activity.map(|t| ago(now, t)).unwrap_or_else(|| "never".into());
        out.push_str(&format!("\n{}  ({} open, last active {})\n", s.project.name, s.open, when));
        for t in &s.doing {
            out.push_str(&format!("  doing  #{} {}\n", t.id, truncate(&t.title, 45)));
        }
    }
    if total > sums.len() {
        out.push_str(&format!("\n...{} more board{}, not active recently\n",
            total - sums.len(), plural(total - sums.len())));
    }
    out
}

/// Errors state what went wrong, what the current state is, and what to do next. A bare
/// failure message costs three follow-up calls; this costs none.
pub fn error(e: &crate::core::Error) -> String {
    use crate::core::Error as E;
    match e {
        E::TaskNotFound { id, project, existing } => {
            let mut s = format!("No task #{id} on board \"{project}\".\n");
            if existing.is_empty() {
                s.push_str("\nThis board has no tasks yet.\n");
            } else {
                s.push_str("\nOn this board:\n");
                for (tid, title) in existing {
                    s.push_str(&format!("  #{tid} {}\n", truncate(title, 50)));
                }
            }
            s
        }
        E::NoteNotFound { id, .. } => format!("No note #{id} on this board.\n"),
        E::ProjectNotFound { query, existing } => {
            let mut s = format!("No board matching \"{query}\".\n\nBoards:\n");
            for p in existing { s.push_str(&format!("  {p}\n")); }
            s
        }
        E::AmbiguousProject { query, candidates } => {
            let mut s = format!("\"{query}\" matches {} boards:\n\n", candidates.len());
            for c in candidates { s.push_str(&format!("  {c}\n")); }
            s.push_str("\nName one exactly.\n");
            s
        }
        E::InvalidValue { field, value, valid } => {
            format!("\"{value}\" is not a valid {field}.\n\nValid values: {valid}\n")
        }
        E::NoProjectContext => {
            "Could not tell which project this is.\n\nPass project explicitly, or run the server \
             with a working directory inside the project.\n".to_string()
        }
        other => format!("{other}\n"),
    }
}

fn plural(n: usize) -> &'static str { if n == 1 { "" } else { "s" } }

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max { return s; }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}
