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
use crate::core::staleness::MissingSubjects;

/// `ago` returns a bare duration ("3mo") for old things but a complete phrase ("just now")
/// for recent ones. These two wrap it so callers never staple a suffix onto the phrase and
/// produce "just now old" or "filed just now ago".
///
/// Age of a thing: `3mo old` / `just now`.
pub fn age(now: i64, ts: i64) -> String {
    let a = ago(now, ts);
    if a == "just now" { a } else { format!("{a} old") }
}

/// Time since an event: `3mo ago` / `just now`.
pub fn since(now: i64, ts: i64) -> String {
    let a = ago(now, ts);
    if a == "just now" { a } else { format!("{a} ago") }
}

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
        out.push_str(&task_line(
            t, snap.highlight == Some(t.id), &snap.blocker_status,
            snap.links_of(t.id), snap.repo_count > 0,
        ));
    }

    // What the cap left out. Stated, never silently dropped -- a board that hides work is
    // worse than one that admits it is showing a slice.
    if !snap.omitted.is_empty() {
        let more = snap.omitted.iter()
            .map(|(s, n)| format!("+{n} {s}"))
            .collect::<Vec<_>>().join(", ");
        out.push_str(&format!("\n  ...{more}\n"));
    }

    // Said only when the cap is actually hiding work AND there is nothing to group by.
    //
    // Those two conditions together are the whole design of this line. `omitted` non-empty
    // means the board is showing a slice; no workstreams means there is no way for it to be
    // the RIGHT slice, because selection then falls back to recency. That is the failure
    // measured in note #16: a cold agent told in detail about work that is not its own.
    //
    // It goes quiet the moment a workstream exists, which is what keeps it from becoming
    // the repeated boilerplate that gets a plugin uninstalled and that a model learns to
    // skip. Suppressed on an after-write snapshot as well (`highlight`): that response is a
    // confirmation that filing landed, and filing has to stay cheap.
    if !snap.omitted.is_empty()
        && snap.workstream.is_none()
        && snap.other_workstreams.is_empty()
        && snap.highlight.is_none()
    {
        out.push_str(
            "\n  Nothing here is grouped, so this is the most recently touched work rather \
             than the work in hand. When the user says what they are working on, pass it to \
             board as `workstream` -- the board narrows to it and new tasks join it.\n",
        );
    }

    if !snap.recent.is_empty() {
        out.push_str("\nrecent\n");
        for e in &snap.recent {
            out.push_str(&event_line(e, snap.now));
        }
    }
    out
}

/// How many workstreams the directory names before collapsing the rest into a count.
///
/// Four, because the directory is paid for on every board call and its whole justification
/// is that it stays ONE line however many workstreams exist. A board with twenty of them
/// must not turn its header into a listing -- that is the "grouped rendering" this design
/// rejected.
const DIRECTORY_LIMIT: usize = 4;

fn header(snap: &BoardSnapshot) -> String {
    let open = snap.open_count();
    let doing = snap.count_of(Status::Doing);
    let mut bits = vec![format!("{open} open")];
    if doing > 0 { bits.push(format!("{doing} doing")); }
    let done = snap.count_of(Status::Done);
    if done > 0 { bits.push(format!("{done} done")); }

    // The scope is named in the header for the same reason the project is: an agent must
    // never have to guess what it is looking at. A board silently showing a subset is the
    // confusion this feature was built to fix, so it would be perverse to reintroduce it.
    let title = match &snap.workstream {
        Some(w) => format!("{} / {}", snap.project.name, w.name),
        None => snap.project.name.clone(),
    };
    let mut out = format!("Board: {}  ({})\n", title, bits.join(", "));

    if !snap.other_workstreams.is_empty() {
        let shown = snap.other_workstreams.iter().take(DIRECTORY_LIMIT)
            .map(|w| format!("{} {}", w.workstream.name, w.open))
            .collect::<Vec<_>>().join(", ");
        let rest = snap.other_workstreams.len().saturating_sub(DIRECTORY_LIMIT);
        let more = if rest > 0 { format!(", +{rest} more") } else { String::new() };
        // Named "other workstreams" even when nothing is scoped, because the list is
        // literally the workstreams other than the current one -- and when there is no
        // current one, telling the agent they exist at all is the point.
        out.push_str(&format!("other workstreams: {shown}{more}\n"));
    }
    out
}

/// One task, one line. The trailing parenthetical carries only what is *not* obvious from
/// the columns -- origin is always shown because it is the instrumentation for whether the
/// agent files work unprompted, and that question needs to stay visible.
///
/// Repos and the Planio ref follow the parenthetical, and unlike tags they are on the line:
/// which checkouts a ticket lives in and which ticket it is are what an agent needs to pick
/// work up at all. Neither costs anything on a board that has none.
///
/// `no repo set` appears only when the board tracks repos (`flag_missing`) and only on open
/// work -- on a board with no repos every task would say it, and a line that always says the
/// same thing is one a reader learns to skip.
fn task_line(
    t: &Task, highlighted: bool, blockers: &[(i64, Status)],
    links: Option<&TaskLinks>, flag_missing: bool,
) -> String {
    let mark = if highlighted { "*" } else { " " };
    let mut meta = vec![t.origin.to_string()];
    if t.task_type != TaskType::Task { meta.insert(0, t.task_type.to_string()); }
    if t.priority != Priority::Normal { meta.push(t.priority.to_string()); }
    if let Some(b) = t.blocked_by {
        // The blocker's status, not just its id. A finished blocker is no longer a reason
        // to skip the task, and the id alone cannot say so -- which left tasks reading as
        // blocked forever. Stated rather than dropped: "was blocked, now clear" explains
        // why the task is sitting in backlog instead of in flight, which a bare line does
        // not. An unknown status (another board's id, or a forgotten task) prints as before.
        meta.push(match blockers.iter().find(|(id, _)| *id == b) {
            Some((_, s)) if !s.is_open() => format!("blocked by #{b} ({s})"),
            _ => format!("blocked by #{b}"),
        });
    }
    let mut tail = String::new();
    match links.map(|l| l.repos.as_slice()).filter(|r| !r.is_empty()) {
        Some(repos) => tail.push_str(&format!(" [{}]", repos.join(", "))),
        None if flag_missing && t.status.is_open() => tail.push_str(" no repo set"),
        None => {}
    }
    if let Some(n) = links.and_then(|l| l.planio) {
        tail.push_str(&format!(" planio {n}"));
    }
    format!("{mark} #{:<4} {:<40} ({}){tail}\n", t.id, truncate(&t.title, 40), meta.join(", "))
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
            // Kept out of HOUSEKEEPING_KINDS on purpose -- starting or finishing a slice of
            // work is project history a cold agent benefits from. That decision only pays
            // off if the line says so: the body is just the name, so the generic fallback
            // below would render a bare "contact-form" with no indication of what happened
            // to it.
            "workstream_created" => format!("started workstream \"{}\"", truncate(&e.body, 50)),
            "workstream_closed"  => format!("closed workstream \"{}\"", truncate(&e.body, 50)),
            // Same reasoning as the workstream kinds: history worth the line, but the body
            // alone ("eee-web (/Users/…)") would not say what happened to it.
            "repo_added"   => format!("added repo {}", truncate(&e.body, 60)),
            "repo_renamed" => format!("renamed repo {}", truncate(&e.body, 60)),
            "repo_removed" => format!("removed repo {}", truncate(&e.body, 60)),
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
    if let Some(n) = d.planio { meta.push(format!("planio #{n}")); }
    meta.push(format!("filed {}", since(d.now, t.created_at)));
    out.push_str(&format!("  {}\n", meta.join(", ")));

    if !d.repos.is_empty() {
        // With paths: this is the response an agent reads when it commits to the work, and
        // the path is what tells it where that work is.
        out.push_str("\nrepos\n");
        for r in &d.repos {
            out.push_str(&format!("  {}  {}\n", r.name, r.path));
        }
    } else if d.board_has_repos && t.status.is_open() {
        // The board only flags it; here, at the moment of starting, it says what to do.
        out.push_str("\nno repo set -- ask the user which repos this touches, then record them \
                      with task_update `repos`. A checkout the board does not have yet is \
                      registered with repo_add.\n");
    }

    if !d.tags.is_empty() {
        // Shown here and nowhere on the board. The board listing is the most expensive
        // space in the product and is paid for on every task_add; tags buy an agent
        // nothing there, since carrying no semantics it must honour is exactly what makes
        // them safe to have. task_show is the response that is allowed to cost more,
        // because the agent has already committed to this one task.
        out.push_str(&format!("\ntags: {}\n", d.tags.join(", ")));
    }
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
            out.push_str(&format!("  #{} {}  ({})\n", n.id, truncate(&n.title, 45), age(d.now, n.updated_at)));
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

/// The first message of a Claude session started from the board.
///
/// Names the board explicitly and tells the agent to pass it: the session starts in the
/// ticket's first repo, whose home can be a different board, and a bare `task_show` there
/// would look for the ticket on the wrong one.
pub fn start_prompt(d: &TaskDetail) -> String {
    let t = &d.task;
    let mut out = format!("Work on ticket #{} from the ai-kanban board \"{}\": {}\n", t.id, d.project.name, t.title);
    if let Some(n) = d.planio {
        out.push_str(&format!("\nIt tracks Planio issue #{n}; read it with the Planio tools for the full requirements.\n"));
    }
    if !t.body.is_empty() {
        out.push_str(&format!("\n{}\n", t.body));
    }
    out.push_str("\nRepos:\n");
    for r in &d.repos {
        out.push_str(&format!("  {}  {}\n", r.name, r.path));
    }
    out.push_str(&format!(
        "\nStart with task_show (task {}, project \"{}\"), move it to doing with task_update when \
         you begin, and record what you learn as you go.\n",
        t.id, d.project.name
    ));
    out
}

/// Recall. Each hit states its kind, its age and its project, and carries a snippet --
/// a list of titles is a search result; a list of snippets is an answer.
pub fn recall(r: &RecallResult, cross_project: bool, missing: &MissingSubjects) -> String {
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
        // Only ever on note hits, and only when the check was calibrated -- see
        // `core::staleness`. Phrased as the fact ("the file is not there") rather than as a
        // verdict ("this note is wrong"), because the file being gone is what is actually
        // known. A note about deleted code may still be the reason the code was deleted.
        if h.kind == HitKind::Note {
            let gone = missing.for_note(h.id);
            if !gone.is_empty() {
                out.push_str(&format!("       (no longer in the repo: {})\n", gone.join(", ")));
            }
        }
    }

    // What the cap left out, phrased as a next step rather than a statistic. A capped list
    // that looks complete sends the reader away believing the store holds nothing more --
    // the same failure the board's `omitted` count exists to prevent.
    if r.omitted > 0 {
        out.push_str(&format!("\n{} more match{} not shown. Narrow the query, or pass a higher limit.\n",
            r.omitted, if r.omitted == 1 { "" } else { "es" }));
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
        let when = s.last_activity.map(|t| since(now, t)).unwrap_or_else(|| "never".into());
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

/// The board listing for a **person** at a terminal, which is why it prints the key that
/// `project_summaries` leaves out.
///
/// Two consumers, two renderers, for the same reason `docs/http-api.md` gives two limits:
/// the agent pays for every character of `project_summaries` on calls it makes constantly,
/// while this is a scrollable page someone asked for. And without the key this listing
/// cannot do its job -- a split board is two rows with the *same name*, so a listing that
/// shows only names makes the very thing it exists to reveal invisible, and leaves the
/// reader with no string they can pass to `merge`.
pub fn project_list(sums: &[ProjectSummary], now: i64) -> String {
    if sums.is_empty() {
        return "No boards yet.\n".to_string();
    }
    let mut out = format!("{} board{}\n", sums.len(), plural(sums.len()));
    // Names that appear more than once are the signature of a split, so say so rather than
    // leaving the reader to notice two identical-looking rows.
    let mut seen = std::collections::HashMap::new();
    for s in sums {
        *seen.entry(s.project.name.to_lowercase()).or_insert(0usize) += 1;
    }
    for s in sums {
        let when = s.last_activity.map(|t| since(now, t)).unwrap_or_else(|| "never".into());
        out.push_str(&format!(
            "\n{}  ({} open, last active {})\n  {}\n",
            s.project.name, s.open, when, s.project.key
        ));
    }
    let split: Vec<&String> = seen.iter().filter(|(_, n)| **n > 1).map(|(k, _)| k).collect();
    if !split.is_empty() {
        out.push_str(&format!(
            "\nMore than one board is called {}. That is what a split board looks like: \n\
             one project remembered as two half-memories. `ai-kanban merge <keep> <gone>` \n\
             joins them -- name them by key, since the names collide.\n",
            split.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")
        ));
    }
    out
}

/// A merge is destructive and irreversible, so its report names what moved and states
/// plainly that one board is gone. A count alone would leave the reader unsure whether the
/// key they used to resolve against still works.
pub fn merge_report(r: &crate::core::merge::MergeReport) -> String {
    // A split board is two rows with the SAME name, which is the common case here -- so
    // naming both sides by name alone would print "Merged widget into widget" exactly when
    // the reader most needs to know which one went. Fall back to the key, which is unique.
    let (gone, kept) = if r.merged.name.eq_ignore_ascii_case(&r.into.name) {
        (r.merged.key.clone(), r.into.key.clone())
    } else {
        (r.merged.name.clone(), r.into.name.clone())
    };
    // Mentioned only when there were any. Most merges repair a board that never had a
    // workstream, and a line reading "and 0 workstreams" is noise on the common path.
    let mut extra = Vec::new();
    if r.workstreams > 0 { extra.push(format!("{} workstream{}", r.workstreams, plural(r.workstreams))); }
    if r.repos > 0 { extra.push(format!("{} repo{}", r.repos, plural(r.repos))); }
    let workstreams = if extra.is_empty() { String::new() } else { format!(" and {}", extra.join(" and ")) };
    format!(
        "Merged \"{gone}\" into \"{kept}\".\n\n\
         Moved {} task{}, {} note{}, {} event{} and {} path{}{workstreams}.\n\
         The board {} no longer exists, and every directory that pointed at it now resolves \
         to \"{kept}\".\n",
        r.tasks, plural(r.tasks),
        r.notes, plural(r.notes),
        r.events, plural(r.events),
        r.paths, plural(r.paths),
        r.merged.key,
    )
}

/// An import reports what it wrote **and what it declined to write**. The second half is
/// the one that matters: a skip here is silent in the data, so a run that restored nothing
/// because every project already existed must not read as a success.
pub fn import_report(r: &crate::core::transfer::ImportReport) -> String {
    let mut out = String::new();
    if r.projects.is_empty() && r.skipped_existing.is_empty() {
        return "The export contained no projects.\n".to_string();
    }
    for p in &r.projects {
        out.push_str(&format!(
            "Imported {}: {} task{}, {} note{}, {} event{}.\n",
            p.name,
            p.tasks, plural(p.tasks),
            p.notes, plural(p.notes),
            p.events, plural(p.events),
        ));
        for path in &p.paths_skipped {
            // Named individually rather than counted: the consequence is that opening that
            // directory keeps landing on the board that already owns it, and only the
            // actual path tells the reader whether that matters.
            out.push_str(&format!("  path already claimed by another board, left alone: {path}\n"));
        }
        for path in &p.repos_skipped {
            // Said, because the consequence is invisible otherwise: the repo is on this board
            // and its tickets keep it, but opening its folder still lands on the other board.
            out.push_str(&format!("  repo already opens on another board here, shared without moving it: {path}\n"));
        }
    }
    for key in &r.skipped_existing {
        out.push_str(&format!("Skipped {key}: a board with this key is already here.\n"));
    }
    if !r.skipped_existing.is_empty() {
        out.push_str(
            "\nImport never merges into an existing board -- deciding which side of a \
             divergent history wins is a separate problem.\n",
        );
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
        E::PathClaimed { path, board, key } => format!(
            "{path} already belongs to board \"{board}\".\n\nA directory resolves to exactly one \
             board, so it cannot join a second one. If both boards are the same work, merge \
             them first:\n\n  ai-kanban merge <this board> {key}\n"
        ),
        E::NoProjectContext => {
            "Could not tell which project this is.\n\nPass project explicitly, or run the server \
             with a working directory inside the project.\n".to_string()
        }
        other => format!("{other}\n"),
    }
}

/// A board's repos: what the agent can name on a ticket, and where each one opens.
///
/// The path is on the line because that is the whole reason an agent asks for this list --
/// it needs to know which directory the work is in. The home board is named only when it is
/// **not** this board, since "opens here" is the unremarkable case and printing it on every
/// row would cost a line's worth of tokens to say nothing.
pub fn repo_menu(board: &str, repos: &[RepoSummary]) -> String {
    let mut out = format!("Board: {board}\n\n");
    if repos.is_empty() {
        out.push_str("No repos on this board yet. Add one with repo_add, passing the path of \
                      the checkout.\n");
        return out;
    }
    let pad = repos.iter().map(|r| r.repo.name.chars().count()).max().unwrap_or(0);
    out.push_str("repos\n");
    for r in repos {
        out.push_str(&format!("  {:pad$}  {}", r.repo.name, r.repo.path));
        if r.home_board != board {
            out.push_str(&format!("  (opens on board \"{}\")", r.home_board));
        }
        if r.total > 0 {
            out.push_str(&format!("  {} ticket{}, {} open", r.total, plural(r.total), r.open));
        }
        out.push('\n');
    }
    out
}

/// Every repo in the store with the board its folder opens on -- the cross-board view, for
/// "is this checkout already registered somewhere?", which no single board can answer.
pub fn repo_directory(repos: &[(Repo, String)]) -> String {
    if repos.is_empty() {
        return "No repos in the store yet.\n".to_string();
    }
    let pad = repos.iter().map(|(r, _)| r.name.chars().count()).max().unwrap_or(0);
    let mut out = format!("{} repo{}\n", repos.len(), plural(repos.len()));
    for (r, home) in repos {
        out.push_str(&format!("  {:pad$}  {}  (opens on {})\n", r.name, r.path, home));
    }
    out
}

fn plural(n: usize) -> &'static str { if n == 1 { "" } else { "s" } }

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max { return s; }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}
