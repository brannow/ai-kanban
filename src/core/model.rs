//! Domain types. Core returns these; it never returns prose.
//!
//! The rule this file exists to enforce: an adapter renders, core computes. If a core
//! function returned `String`, every future consumer (the HTTP API, a web UI, a dedicated
//! human tool) would have to parse text back into objects.

use serde::{Deserialize, Serialize};

/// Generates an enum plus its string mapping, so the SQL CHECK constraint and the Rust
/// type can never drift apart, and every parse failure can list what *was* valid --
/// the design law's "errors self-correct", made structural.
macro_rules! sql_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }, default = $def:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }

            /// `Err` carries the full list of valid values so the caller can build a
            /// message that corrects the mistake instead of just reporting it.
            pub fn parse(s: &str) -> Result<Self, String> {
                match s { $($s => Ok($name::$variant),)+ _ => Err(Self::valid_values()) }
            }

            pub fn valid_values() -> String {
                Self::ALL.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
            }
        }

        impl Default for $name { fn default() -> Self { $name::$def } }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                let s = v.as_str()?;
                $name::parse(s).map_err(|_| rusqlite::types::FromSqlError::InvalidType)
            }
        }

        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::Borrowed(self.as_str().into()))
            }
        }
    };
}

sql_enum!(
    /// `archived` is terminal and hidden from the default board. There is deliberately no
    /// `next` -- priority covers it.
    Status { Backlog => "backlog", Doing => "doing", Blocked => "blocked", Done => "done", Archived => "archived" },
    default = Backlog
);

sql_enum!(
    TaskType { Task => "task", Bug => "bug", Idea => "idea", Chore => "chore" },
    default = Task
);

sql_enum!(
    /// Instrumentation for "does the agent file work unprompted". Defaults to `Agent`.
    Origin { User => "user", Agent => "agent" },
    default = Agent
);

sql_enum!(
    /// Words rather than numbers, so nobody has to know whether 1 means urgent or trivial.
    Priority { Low => "low", Normal => "normal", High => "high", Urgent => "urgent" },
    default = Normal
);

sql_enum!(
    Actor { User => "user", Agent => "agent", System => "system" },
    default = Agent
);

impl Status {
    /// Board display order. Not a DB concern -- it is presentation, but it is the *same*
    /// presentation for every adapter, so it lives with the type rather than in one renderer.
    pub fn board_rank(self) -> u8 {
        match self { Status::Doing => 0, Status::Blocked => 1, Status::Backlog => 2, Status::Done => 3, Status::Archived => 4 }
    }
    /// Counted as "open" in board headers and remainder counts.
    pub fn is_open(self) -> bool {
        matches!(self, Status::Backlog | Status::Doing | Status::Blocked)
    }
}

impl Priority {
    pub fn rank(self) -> u8 {
        match self { Priority::Urgent => 0, Priority::High => 1, Priority::Normal => 2, Priority::Low => 3 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    /// Stable identity: git remote URL when there is one, else the repo root path.
    pub key: String,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub body: String,
    pub status: Status,
    pub task_type: TaskType,
    pub origin: Origin,
    pub priority: Priority,
    /// Annotation only -- `status` is authoritative. The two may disagree.
    pub blocked_by: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Bumped on every update. The token a caller passes back to prove it is changing the
    /// row it actually read -- see `TaskPatch::expected_version`.
    pub version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub project_id: i64,
    pub task_id: Option<i64>,
    pub ts: i64,
    pub actor: Actor,
    pub kind: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub project_id: i64,
    pub task_id: Option<i64>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub paths: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Bumped on every update. See `NotePatch::expected_version`.
    pub version: i64,
}

// ---------------------------------------------------------------------------
// Workstreams
// ---------------------------------------------------------------------------

/// A named slice of work inside one board -- a feature, an upgrade, a migration.
///
/// It is a grouping dimension, never an identity. Tasks in different workstreams share a
/// board, so `blocked_by` still works across them and nothing has to be merged when a
/// workstream finishes. See `migrations/005_workstreams.sql` for why that matters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workstream {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub created_at: i64,
    /// Set when the workstream is closed. Hides it from the directory; its tasks are
    /// untouched and still counted by status.
    pub closed_at: Option<i64>,
}

impl Workstream {
    pub fn is_open(&self) -> bool { self.closed_at.is_none() }
}

/// One line of the directory: a workstream and how much open work it holds.
///
/// Counts rather than rows. The directory has to stay one line no matter how many
/// workstreams exist, because it is paid for on every board call -- rendering each
/// workstream's tasks is the "grouped rendering" this design deliberately does not do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkstreamSummary {
    pub workstream: Workstream,
    pub open: usize,
}

// ---------------------------------------------------------------------------
// Board query + snapshot
// ---------------------------------------------------------------------------

/// How much board to return.
///
/// This exists as a type, with defaults, because response volume is a design constraint
/// rather than a detail. Returning the whole board on every mutation is correct for a
/// 10-task project and wrong for a year-old one with 200 tasks -- and an expensive tool
/// is the one a shortcutting agent routes around, which is the exact failure this project
/// exists to fix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardQuery {
    /// Restrict to these statuses. Empty = the open ones (backlog, doing, blocked).
    pub status: Vec<Status>,
    /// Max tasks listed. Anything beyond is reported as a count, never dropped silently.
    pub limit: usize,
    /// Max entries in the `recent` section.
    pub recent_limit: usize,
    /// Restrict to one workstream. `None` means no workstream filtering at all, which is
    /// both the pre-005 behaviour and what a board with no workstreams still does.
    ///
    /// Set, it selects that workstream's tasks **plus unscoped ones** -- see `tasks_in`
    /// for why unscoped work is never hidden.
    pub workstream: Option<i64>,
}

/// Which board the caller means. Resolved by the *adapter* into a `project_id` (or into a
/// list of summaries); core functions take an explicit id, so there is exactly one way to
/// ask core a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// The project resolved from the caller's location.
    Current,
    /// Named explicitly -- by key, by name, or by path.
    Named(String),
    /// Every project. Renders as per-project summaries, never as a concatenated task list:
    /// every open task across every project is not a board.
    All,
}

impl BoardQuery {
    /// The caps a mutation response uses. Deliberately small: this is the response an
    /// agent pays for on every single `task_add`.
    pub const MUTATION_LIMIT: usize = 12;
    pub const MUTATION_RECENT: usize = 5;
    /// The caps an explicit `board` call uses -- the agent asked to look, so show more.
    pub const BOARD_LIMIT: usize = 30;
    pub const BOARD_RECENT: usize = 8;

    pub fn board() -> Self {
        Self { status: vec![], limit: Self::BOARD_LIMIT, recent_limit: Self::BOARD_RECENT, workstream: None }
    }

    pub fn after_mutation() -> Self {
        Self { status: vec![], limit: Self::MUTATION_LIMIT, recent_limit: Self::MUTATION_RECENT, workstream: None }
    }
    pub fn with_status(mut self, status: Vec<Status>) -> Self { self.status = status; self }
    pub fn with_limit(mut self, limit: usize) -> Self { self.limit = limit; self }
    pub fn with_workstream(mut self, w: Option<i64>) -> Self { self.workstream = w; self }
}

/// The standard response shape. Always leads with the resolved project, so the agent never
/// has to speculate about which board it is looking at.
#[derive(Debug, Clone, Serialize)]
pub struct BoardSnapshot {
    pub project: Project,
    pub tasks: Vec<Task>,
    /// Per status: how many exist beyond what `tasks` lists. Stands in for the remainder
    /// rather than dropping it.
    pub omitted: Vec<(Status, usize)>,
    pub counts: Vec<(Status, usize)>,
    pub recent: Vec<Event>,
    /// Set when this snapshot followed a write, so the renderer can mark the changed row.
    pub highlight: Option<i64>,
    /// The workstream this snapshot is scoped to, if any. Rendered in the header for the
    /// same reason the project is: the agent must never have to guess what it is looking
    /// at. A board that silently shows a subset is worse than one that shows everything.
    pub workstream: Option<Workstream>,
    /// The other open workstreams on this board, with their open counts. What the scope
    /// excluded, stated rather than hidden -- the same rule `omitted` follows for statuses.
    pub other_workstreams: Vec<WorkstreamSummary>,
    pub now: i64,
}

impl BoardSnapshot {
    pub fn count_of(&self, s: Status) -> usize {
        self.counts.iter().find(|(k, _)| *k == s).map(|(_, n)| *n).unwrap_or(0)
    }
    pub fn open_count(&self) -> usize {
        self.counts.iter().filter(|(s, _)| s.is_open()).map(|(_, n)| n).sum()
    }
}

/// `board(project: "all")` -- a per-project summary, not a merged task list.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub project: Project,
    pub open: usize,
    pub doing: Vec<Task>,
    pub last_activity: Option<i64>,
}

/// One task with everything needed to resume it: full history and the notes attached to it.
#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub project: Project,
    pub task: Task,
    pub blocker: Option<Task>,
    pub blocking: Vec<Task>,
    pub events: Vec<Event>,
    pub notes: Vec<Note>,
    pub now: i64,
}

// ---------------------------------------------------------------------------
// Recall
// ---------------------------------------------------------------------------

/// What kind of thing matched. A task hit, an event hit and a note hit are not the same
/// thing and must never render as if they were -- a note is a claim about the code, an
/// event is something that happened, a task is work. Flattening them loses the distinction
/// that makes the answer usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HitKind { Note, Task, Event }

#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub kind: HitKind,
    /// The id of the matched entity *itself*, always in that entity's own id space.
    /// Mixing spaces here (an event hit reporting its task's id) would make `#N` in a
    /// rendered result sometimes a valid `task_show` input and sometimes a pointer at an
    /// unrelated task -- breaking the rule that one tool's output is another's input.
    pub id: i64,
    /// The task this hit hangs off, when there is one. This is the reference to follow;
    /// `id` is what the hit *is*.
    pub task_id: Option<i64>,
    /// Stated on every hit. Without it a cross-project result is speculative in exactly the
    /// way the board header exists to prevent.
    pub project: String,
    pub title: String,
    /// Text around the match. A hit without surrounding context is a title, not an answer.
    pub snippet: String,
    pub ts: i64,
    /// FTS5 rank -- more negative is a better match.
    pub score: f64,
    /// Only set for tasks, so a hit can say "done, 2mo ago" instead of just naming the task.
    pub status: Option<Status>,
}

/// Recall's full result, including what to say when there are no hits.
#[derive(Debug, Clone, Serialize)]
pub struct RecallResult {
    pub query: String,
    pub hits: Vec<RecallHit>,
    /// Which projects were searched, for the response header.
    pub scope: String,
    /// Populated so an empty result can orient rather than just report nothing: per the
    /// design law, "no hits" should say what *is* in the store and suggest widening.
    pub available: StoreOverview,
    pub now: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreOverview {
    pub notes: usize,
    pub tasks: usize,
    pub events: usize,
    pub projects: usize,
}
