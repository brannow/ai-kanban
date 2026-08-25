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

    #[error("no project matching {query:?}")]
    ProjectNotFound { query: String, existing: Vec<String> },

    /// Ambiguity is never resolved silently -- the caller lists the candidates and asks.
    #[error("{query:?} matches {} projects", candidates.len())]
    AmbiguousProject { query: String, candidates: Vec<String> },

    #[error("invalid {field}: {value:?}")]
    InvalidValue { field: &'static str, value: String, valid: String },

    #[error("could not determine a project directory")]
    NoProjectContext,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
