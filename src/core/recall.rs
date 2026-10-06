//! Full-text recall across notes, events and tasks.
//!
//! # Why this file gets more care than its size suggests
//!
//! Priority #1 is that an agent starting cold knows what was learned before. A store is
//! only as good as its retrieval, so this is where the design is most likely to disappoint
//! in practice: everything can be written correctly and still be unfindable.
//!
//! Three things follow from that, and they are the whole design of this module:
//!
//!  * Hits keep their kind. A note is a claim about the code, an event is something that
//!    happened, a task is work. Merged into one undifferentiated list they stop answering
//!    the question that was asked.
//!  * Hits carry a snippet. A list of titles is a search result; a list of snippets is an
//!    answer. The difference decides whether the agent follows up with three more calls.
//!  * Empty results orient. "No hits" that also says what the store *does* contain, and
//!    that cross-project search exists, turns a dead end into a next step.

use crate::core::error::Result;
use crate::core::model::*;
use crate::core::store::{now, Store};
use rusqlite::params;

/// Default cap. Recall is a read the agent asked for, so it can afford more than a
/// mutation response -- but not unboundedly more.
pub const DEFAULT_LIMIT: usize = 10;

/// Cap when whole texts are asked for. One session hand-off runs to several hundred
/// tokens, so ten of them would cost more than the board itself; the agent asking for the
/// full text usually wants the newest one or two, and the omitted count says if there is more.
pub const FULL_LIMIT: usize = 3;
/// Ceiling on an explicit limit in full mode. The body is what the agent asked to pay for,
/// but an unbounded body count is a call that can silently cost tens of thousands of tokens.
pub const FULL_MAX: usize = 10;

/// Turns whatever the agent typed into a valid FTS5 query.
///
/// This exists because FTS5's MATCH syntax is a query language, not a string: `auth-loop`
/// is a NOT expression, a stray quote is a syntax error, and `AND`/`OR`/`NEAR` are
/// keywords. Passing raw user text through means natural phrasings fail with a parser
/// error -- and the agent would have to learn FTS5 syntax to avoid it, which is exactly the
/// "needs knowledge of the system's internals" failure the design law forbids.
///
/// Every term is quoted, which makes it a literal, and terms are ANDed together.
///
/// A word glued together by punctuation -- `hand-off`, `go-live`, `2026-10-06`,
/// `src/render.rs` -- stays one term, as an FTS5 phrase of its parts. Split into loose words
/// instead, `hand-off` matched any text holding "hand" and "off" anywhere, and those hits
/// crowded out the hand-offs themselves. The tokenizer stores the parts adjacently, so the
/// phrase finds exactly the glued word.
///
/// Bare FTS5 operator keywords are dropped rather than quoted. An agent typing
/// "middleware AND auth" means the boolean, not a document containing the word "and" --
/// and since every term is ANDed regardless, treating them as noise is both closer to the
/// intent and impossible to get wrong. Inside a glued word they are ordinary words
/// (`rock-and-roll`), and a quoted phrase is a literal anyway.
pub fn fts_query(input: &str) -> Option<String> {
    const OPERATORS: [&str; 4] = ["AND", "OR", "NOT", "NEAR"];
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|word| word
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>())
        .filter(|parts| match parts.as_slice() {
            [] => false,
            [one] => !OPERATORS.contains(&one.to_uppercase().as_str()),
            _ => true,
        })
        .map(|parts| format!("\"{}\"", parts.join(" ")))
        .collect();
    if terms.is_empty() { None } else { Some(terms.join(" AND ")) }
}

/// `None` for `project_id` searches every project -- the cross-project recall that no
/// per-repo board can offer ("have I hit this same nginx thing somewhere before?").
pub struct RecallQuery<'a> {
    pub text: &'a str,
    pub project_id: Option<i64>,
    pub limit: usize,
}

fn kind_rank(k: HitKind) -> u8 {
    match k { HitKind::Note => 0, HitKind::Task => 1, HitKind::Event => 2 }
}

impl Store {
    pub fn recall(&self, q: &RecallQuery<'_>) -> Result<RecallResult> {
        let scope = match q.project_id {
            Some(id) => self.project(id)?.name,
            None => "all projects".to_string(),
        };
        let available = self.overview(q.project_id)?;

        let Some(match_expr) = fts_query(q.text) else {
            return Ok(RecallResult {
                query: q.text.into(), hits: vec![], scope, available, omitted: 0, now: now(),
            });
        };

        let total = self.match_total(&match_expr, q.project_id)?;

        let mut hits = Vec::new();
        hits.extend(self.recall_notes(&match_expr, q.project_id, q.limit)?);
        hits.extend(self.recall_tasks(&match_expr, q.project_id, q.limit)?);
        hits.extend(self.recall_events(&match_expr, q.project_id, q.limit)?);

        // Sort by KIND first, then by rank within that kind.
        //
        // The tempting version -- merge everything and sort by raw `rank` -- looks more
        // principled and is wrong: bm25 scores are computed per FTS table against that
        // table's own corpus statistics and column count, so a note's -1.8 and an event's
        // -1.8 are not the same quantity. Comparing them directly interleaves on a number
        // that has no shared meaning.
        //
        // Kind order is a deliberate editorial choice, not a fallback: a note is a durable
        // claim about the code, which is what "what do we know about X" is asking for. A
        // task says whether the work was already done. An event is the narrowest -- one
        // moment, usually only meaningful next to the thing it happened to.
        hits.sort_by(|a, b| kind_rank(a.kind).cmp(&kind_rank(b.kind))
            .then(a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .then(b.ts.cmp(&a.ts)));
        hits.truncate(q.limit);

        let omitted = total.saturating_sub(hits.len());
        Ok(RecallResult { query: q.text.into(), hits, scope, available, omitted, now: now() })
    }

    /// Total rows matching, so a truncated result can say what it left out.
    ///
    /// Counted, not inferred from what came back. The cap is applied TWICE -- each of the
    /// three searches takes `limit` rows before the merged list is truncated to `limit`
    /// again -- so the number of hits in hand is a lower bound on the matches and nothing
    /// more. Publishing a lower bound as though it were the count produces a number that
    /// gets believed and is wrong whenever it matters.
    ///
    /// The event filters are BUILT from the same constants `recall_events` builds them from,
    /// never written out again here. Counting rows the search can never return would report
    /// omissions that do not exist, and a duplicated literal is how the two drift apart.
    fn match_total(&self, m: &str, project: Option<i64>) -> Result<usize> {
        let housekeeping = crate::core::event::housekeeping_filter().replace("kind", "e.kind");
        let duplicates = crate::core::event::duplicate_title_filter().replace("kind", "e.kind");
        let n: i64 = self.conn.query_row(&format!(
            "SELECT (SELECT count(*) FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
                      WHERE notes_fts MATCH ?1 AND (?2 IS NULL OR n.project_id = ?2))
                  + (SELECT count(*) FROM tasks_fts JOIN tasks t ON t.id = tasks_fts.rowid
                      WHERE tasks_fts MATCH ?1 AND (?2 IS NULL OR t.project_id = ?2))
                  + (SELECT count(*) FROM events_fts JOIN events e ON e.id = events_fts.rowid
                      WHERE events_fts MATCH ?1 AND (?2 IS NULL OR e.project_id = ?2)
                        AND {duplicates} AND {housekeeping})"
        ), params![m, project], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    fn recall_notes(&self, m: &str, project: Option<i64>, limit: usize) -> Result<Vec<RecallHit>> {
        let mut st = self.conn.prepare(
            "SELECT n.id, p.name, n.title,
                    snippet(notes_fts, -1, '>>', '<<', '...', 20),
                    n.updated_at, notes_fts.rank, n.task_id
               FROM notes_fts
               JOIN notes n ON n.id = notes_fts.rowid
               JOIN projects p ON p.id = n.project_id
              WHERE notes_fts MATCH ?1 AND (?2 IS NULL OR n.project_id = ?2)
              ORDER BY notes_fts.rank, n.id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![m, project, limit as i64], |r| {
            Ok(RecallHit {
                kind: HitKind::Note,
                id: r.get(0)?, task_id: r.get(6)?, project: r.get(1)?, title: r.get(2)?,
                snippet: r.get(3)?, ts: r.get(4)?, score: r.get(5)?, status: None, body: None,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn recall_tasks(&self, m: &str, project: Option<i64>, limit: usize) -> Result<Vec<RecallHit>> {
        let mut st = self.conn.prepare(
            "SELECT t.id, p.name, t.title,
                    snippet(tasks_fts, -1, '>>', '<<', '...', 20),
                    t.updated_at, tasks_fts.rank, t.status
               FROM tasks_fts
               JOIN tasks t ON t.id = tasks_fts.rowid
               JOIN projects p ON p.id = t.project_id
              WHERE tasks_fts MATCH ?1 AND (?2 IS NULL OR t.project_id = ?2)
              ORDER BY tasks_fts.rank, t.id DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![m, project, limit as i64], |r| {
            Ok(RecallHit {
                kind: HitKind::Task,
                id: r.get(0)?, task_id: Some(r.get(0)?), project: r.get(1)?, title: r.get(2)?,
                snippet: r.get(3)?, ts: r.get(4)?, score: r.get(5)?, status: Some(r.get(6)?), body: None,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Events have no title of their own, so the hit borrows the task's -- an event line
    /// reading only "root cause was middleware ordering" with no subject is half an answer.
    ///
    /// `created` and `note_added` events are excluded. Their bodies are copies of the task
    /// or note title, so including them makes every entity match twice: once as itself and
    /// once as an event repeating its own name. Housekeeping kinds go too: a `path_learned`
    /// body is a filesystem path, so searching for a directory name would return the record
    /// of visiting it instead of the work done there. That is pure duplication in a response
    /// whose budget is the thing keeping the board cheap enough to use. The event kinds
    /// that survive are the ones carrying reasoning the entity does not already hold --
    /// status transitions, logs, and the previous value of an edited note.
    fn recall_events(&self, m: &str, project: Option<i64>, limit: usize) -> Result<Vec<RecallHit>> {
        let housekeeping = crate::core::event::housekeeping_filter().replace("kind", "e.kind");
        let duplicates = crate::core::event::duplicate_title_filter().replace("kind", "e.kind");
        let mut st = self.conn.prepare(&format!(
            "SELECT e.id, p.name, COALESCE(t.title, ''),
                    snippet(events_fts, 0, '>>', '<<', '...', 20),
                    e.ts, events_fts.rank, e.task_id
               FROM events_fts
               JOIN events e ON e.id = events_fts.rowid
               JOIN projects p ON p.id = e.project_id
               LEFT JOIN tasks t ON t.id = e.task_id
              WHERE events_fts MATCH ?1 AND (?2 IS NULL OR e.project_id = ?2)
                AND {duplicates} AND {housekeeping}
              ORDER BY events_fts.rank, e.id DESC LIMIT ?3"
        ))?;
        let rows = st.query_map(params![m, project, limit as i64], |r| {
            let task_id: Option<i64> = r.get(6)?;
            let title: String = r.get(2)?;
            Ok(RecallHit {
                kind: HitKind::Event,
                id: r.get(0)?,
                task_id,
                project: r.get(1)?,
                title: if title.is_empty() { "project log".into() } else { title },
                snippet: r.get(3)?, ts: r.get(4)?, score: r.get(5)?, status: None, body: None,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Loads the whole text of every hit -- recall's `full` mode.
    ///
    /// A second step after the search, not a flag on it: it touches only the hits that
    /// survived the cap, and the search stays one code path. Looking rows up by bare id is
    /// safe here, unlike the general rule, because the ids are the search's own output and
    /// the search already applied the project scope.
    pub fn fill_bodies(&self, r: &mut RecallResult) -> Result<()> {
        for h in &mut r.hits {
            let sql = match h.kind {
                HitKind::Note => "SELECT body FROM notes WHERE id = ?1",
                HitKind::Task => "SELECT body FROM tasks WHERE id = ?1",
                HitKind::Event => "SELECT body FROM events WHERE id = ?1",
            };
            h.body = Some(self.conn.query_row(sql, params![h.id], |row| row.get(0))?);
        }
        Ok(())
    }

    /// Feeds the empty-result case, so "nothing found" can still be useful.
    pub fn overview(&self, project: Option<i64>) -> Result<StoreOverview> {
        let one = |sql: &str| -> Result<usize> {
            Ok(self.conn.query_row(sql, params![project], |r| r.get::<_, i64>(0))? as usize)
        };
        Ok(StoreOverview {
            notes: one("SELECT COUNT(*) FROM notes WHERE (?1 IS NULL OR project_id = ?1)")?,
            tasks: one("SELECT COUNT(*) FROM tasks WHERE (?1 IS NULL OR project_id = ?1)")?,
            events: one("SELECT COUNT(*) FROM events WHERE (?1 IS NULL OR project_id = ?1)")?,
            projects: self.conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get::<_, i64>(0))? as usize,
        })
    }
}
