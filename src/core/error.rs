use thiserror::Error;

/// Core errors carry *structured* context, not sentences. The adapter turns them into
/// prose that states what went wrong, what the current state is, and what to do next --
/// keeping that phrasing in core would bake one consumer's output format into the domain.
#[derive(Debug, Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Carries what *does* exist, so the caller can render a correcting message.
    #[error("no task #{id}")]
    TaskNotFound { id: i64, project: String, existing: Vec<(i64, String)> },

    #[error("no note #{id}")]
    NoteNotFound { id: i64, project: String },

    /// No `existing` list: the to-do list is the person's own and short, and they are
    /// looking at it. Echoing it back in an error would be telling them what they can see.
    #[error("no to-do #{id}")]
    TodoNotFound { id: i64 },

    #[error("no project matching {query:?}")]
    ProjectNotFound { query: String, existing: Vec<String> },

    /// Ambiguity is never resolved silently -- the caller lists the candidates and asks.
    #[error("{query:?} matches {} projects", candidates.len())]
    AmbiguousProject { query: String, candidates: Vec<String> },

    #[error("invalid {field}: {value:?}")]
    InvalidValue { field: &'static str, value: String, valid: String },

    /// A directory another board already resolves. One directory maps to exactly one board,
    /// so it cannot join a second one -- carries the owner so the caller can name it and
    /// point at `merge`.
    #[error("{path} already belongs to board {board:?}")]
    PathClaimed { path: String, board: String, key: String },

    #[error("could not determine a project directory")]
    NoProjectContext,

    /// The walk found nothing, and the starting directory is one that sits above every
    /// project -- `$HOME`, a directory above it, a filesystem root, the temp dir -- so it is
    /// never a board by default.
    #[error("{path} is not a project directory")]
    SharedDirectory { path: String },

    /// Someone else changed the row between the caller reading it and writing it back.
    /// Carries both versions so the caller can say what happened rather than just refusing.
    #[error("#{id} changed since you read it (you had v{expected}, it is now v{actual})")]
    Conflict { id: i64, expected: i64, actual: i64 },

    /// A request the server refuses whoever sends it -- today, starting a session from
    /// anywhere but the board page itself.
    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
