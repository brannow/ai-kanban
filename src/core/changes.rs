//! What changed on a board between two moments: `board(since, until)`.
//!
//! The board's `recent` section answers "what happened lately" with a fixed count, so one busy
//! day pushes everything older out of it. The question an agent coming back after a weekend
//! actually has is bounded by time, not by count -- "what happened since Friday" -- and nothing
//! else answered it (#1172).
//!
//! Built on the events table rather than on `tasks.updated_at`: an event says WHAT changed,
//! and a `log` deliberately leaves `updated_at` alone. Each task appears once, as of its newest
//! event in the range; project-level entries (decisions, releases, session summaries) appear as
//! themselves, because they are often the most important lines and belong to no task.

use crate::core::error::{Error, Result};
use crate::core::event::{housekeeping_filter, row_to_event, EVENT_COLS};
use crate::core::model::{Event, Handles, Project, Status};
use crate::core::store::Store;
use rusqlite::{params, OptionalExtension};

/// What `board(since, until)` was asked for, resolved to epoch seconds.
#[derive(Debug, Clone)]
pub struct ChangeQuery {
    /// Inclusive.
    pub since: i64,
    /// Exclusive, so a date-only `until` can be "the next midnight" and still mean that day.
    pub until: i64,
    /// Only tasks whose CURRENT status is one of these. Empty = no filter. A status filter also
    /// drops project-level entries: they have no status, and "what got finished this week" is
    /// not answered by a release note.
    pub status: Vec<Status>,
    pub limit: usize,
    /// Same scope rules as the board's `recent` section (`recent_events_in`).
    pub workstream: Option<i64>,
}

impl ChangeQuery {
    /// Rows, across one board or all of them. Each is one line of about 30 tokens, so this is
    /// about what an explicit `board` call already costs (`tests/budget.rs`).
    pub const LIMIT: usize = 30;
    /// Used when only `until` is given: the agreed default window (#1172).
    pub const DEFAULT_WINDOW: i64 = 7 * 86400;
}

/// One task's activity in the range.
#[derive(Debug, Clone)]
pub struct TaskChange {
    pub task_id: i64,
    pub title: String,
    /// None when the task is no longer on this board (forgotten, or moved to another one).
    pub status: Option<Status>,
    /// The newest event in the range. Its time is the row's time and its writer the "latest
    /// editor": history is append-only, so the newest entry is the latest edit.
    pub last: Event,
    /// What the row says changed: the newest status transition in the range if there was
    /// one, else `last`. A task that went to testing and then got a log must still read as
    /// "-> testing" -- the transition is the line's point, and the log would bury it.
    pub shown: Event,
    /// How many events the task had in the range, `last` included.
    pub events: usize,
}

#[derive(Debug, Clone)]
pub enum Change {
    Task(TaskChange),
    /// A project-level event: a log, a note, a workstream started.
    Entry(Event),
}

impl Change {
    pub fn newest(&self) -> &Event {
        match self {
            Change::Task(t) => &t.last,
            Change::Entry(e) => e,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Changes {
    pub since: i64,
    pub until: i64,
    pub now: i64,
    /// Local time's offset from UTC, in seconds, so the renderer can print "Nov 2 14:00" as the
    /// person reads a clock without core rendering prose or render needing a timezone library.
    pub utc_offset: i64,
    /// Newest first, capped.
    pub rows: Vec<Change>,
    /// Rows the cap left out. Stated, never dropped silently.
    pub omitted: usize,
    /// For a view across every board: the boards in the order their sections appear. Empty for
    /// one board.
    pub projects: Vec<Project>,
    pub handles: Handles,
}

impl Store {
    /// What changed on one board in `[since, until)`.
    pub fn changes(&self, project_id: i64, q: &ChangeQuery) -> Result<Changes> {
        let rows = self.change_rows(project_id, q)?;
        self.finish_changes(rows, q, vec![])
    }

    /// What changed on every board. One cap for all of them, filled newest first: a cap per
    /// board would let whichever board comes first take the whole view.
    pub fn changes_everywhere(&self, q: &ChangeQuery) -> Result<Changes> {
        let mut st = self.conn.prepare(&format!(
            "SELECT DISTINCT project_id FROM events WHERE ts >= ?1 AND ts < ?2 AND {}",
            housekeeping_filter()
        ))?;
        let pids = st.query_map(params![q.since, q.until], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // A workstream belongs to one board; across all of them there is no scope to apply.
        let q_all = ChangeQuery { workstream: None, ..q.clone() };
        let mut rows = Vec::new();
        for pid in pids {
            rows.extend(self.change_rows(pid, &q_all)?);
        }
        sort_newest_first(&mut rows);
        // Sections in the order of each board's newest row, which is the order they are read.
        let mut projects: Vec<Project> = Vec::new();
        for r in rows.iter().take(q.limit) {
            let pid = r.newest().project_id;
            if !projects.iter().any(|p| p.id == pid) {
                projects.push(self.project(pid)?);
            }
        }
        self.finish_changes(rows, q, projects)
    }

    fn finish_changes(&self, mut rows: Vec<Change>, q: &ChangeQuery, projects: Vec<Project>) -> Result<Changes> {
        sort_newest_first(&mut rows);
        let omitted = rows.len().saturating_sub(q.limit);
        rows.truncate(q.limit);
        // Only the events that are rendered: `handles` looks each one up, and a year-wide
        // range can hold thousands.
        let mut texts: Vec<(&str, i64)> = Vec::new();
        for r in &rows {
            match r {
                Change::Task(t) => {
                    texts.push(("event", t.last.id));
                    texts.push(("event", t.shown.id));
                }
                Change::Entry(e) => texts.push(("event", e.id)),
            }
        }
        let handles = self.handles(&texts)?;
        Ok(Changes {
            since: q.since,
            until: q.until,
            now: crate::core::now(),
            utc_offset: self.utc_offset()?,
            rows,
            omitted,
            projects,
            handles,
        })
    }

    /// Every change on one board in the range, uncapped and unsorted.
    fn change_rows(&self, project_id: i64, q: &ChangeQuery) -> Result<Vec<Change>> {
        let mut out = Vec::new();
        for (last, events) in self.newest_per_task(project_id, q)? {
            let task_id = last.task_id.expect("selected with task_id IS NOT NULL");
            // Joined on project_id too: the store is global, and a task moved to another board
            // must not lend this board's row its new home's title or status.
            let task: Option<(String, Status)> = self.conn
                .query_row(
                    "SELECT title, status FROM tasks WHERE id = ?1 AND project_id = ?2",
                    params![task_id, project_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if !q.status.is_empty() && !task.as_ref().is_some_and(|(_, s)| q.status.contains(s)) {
                continue;
            }
            let (title, status) = match task {
                Some((t, s)) => (t, Some(s)),
                // Gone from this board: its `created` event still carries the title.
                None => (self.created_title(project_id, task_id)?.unwrap_or_default(), None),
            };
            let shown = self.newest_transition(project_id, task_id, q)?.unwrap_or_else(|| last.clone());
            out.push(Change::Task(TaskChange { task_id, title, status, last, shown, events }));
        }
        if q.status.is_empty() {
            let sql = format!(
                "SELECT {EVENT_COLS} FROM events
                  WHERE project_id = ?1 AND task_id IS NULL AND ts >= ?2 AND ts < ?3 AND {}",
                housekeeping_filter()
            );
            let mut st = self.conn.prepare(&sql)?;
            let entries = st.query_map(params![project_id, q.since, q.until], row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(entries.into_iter().map(Change::Entry));
        }
        Ok(out)
    }

    /// The newest event per task in the range, with the task's event count.
    ///
    /// `workstream_id` is named only when a workstream is set: on a store this binary has not
    /// migrated the column does not exist, and naming it anywhere fails the query (note #17).
    fn newest_per_task(&self, project_id: i64, q: &ChangeQuery) -> Result<Vec<(Event, usize)>> {
        let cols = EVENT_COLS.split(", ").map(|c| format!("e.{c}")).collect::<Vec<_>>().join(", ");
        let scope = if q.workstream.is_some() {
            "AND EXISTS (SELECT 1 FROM tasks t WHERE t.id = e.task_id
                          AND (t.workstream_id = ?4 OR t.workstream_id IS NULL))"
        } else {
            "AND (?4 IS NULL)"
        };
        // The id tie-break: timestamps are whole seconds, and two events in one second must
        // still pick the same "newest" on every call.
        let sql = format!(
            "SELECT {} , n FROM (
                SELECT {cols},
                       ROW_NUMBER() OVER (PARTITION BY e.task_id ORDER BY e.ts DESC, e.id DESC) AS rn,
                       COUNT(*) OVER (PARTITION BY e.task_id) AS n
                  FROM events e
                 WHERE e.project_id = ?1 AND e.task_id IS NOT NULL AND e.ts >= ?2 AND e.ts < ?3
                   AND {} {scope}
             ) WHERE rn = 1",
            EVENT_COLS,
            housekeeping_filter().replace("kind ", "e.kind "),
        );
        let run = |sql: &str, ws: Option<i64>| -> rusqlite::Result<Vec<(Event, usize)>> {
            let mut st = self.conn.prepare(sql)?;
            let rows = st.query_map(params![project_id, q.since, q.until, ws], |r| {
                Ok((row_to_event(r)?, r.get::<_, i64>(7)? as usize))
            })?;
            rows.collect()
        };
        match run(&sql, q.workstream) {
            Ok(v) => Ok(v),
            // The same tolerance as `recent_events_in`: an unmigrated store has no
            // `tasks.workstream_id`, and an unscoped answer beats none.
            Err(e) if q.workstream.is_some() && e.to_string().contains("no such column") => {
                self.newest_per_task(project_id, &ChangeQuery { workstream: None, ..q.clone() })
            }
            Err(e) => Err(e.into()),
        }
    }

    fn newest_transition(&self, project_id: i64, task_id: i64, q: &ChangeQuery) -> Result<Option<Event>> {
        Ok(self.conn
            .query_row(
                &format!(
                    "SELECT {EVENT_COLS} FROM events
                      WHERE project_id = ?1 AND task_id = ?2 AND kind LIKE 'status:%' AND ts >= ?3 AND ts < ?4
                      ORDER BY ts DESC, id DESC LIMIT 1"
                ),
                params![project_id, task_id, q.since, q.until],
                row_to_event,
            )
            .optional()?)
    }

    fn created_title(&self, project_id: i64, task_id: i64) -> Result<Option<String>> {
        Ok(self.conn
            .query_row(
                "SELECT body FROM events WHERE project_id = ?1 AND task_id = ?2 AND kind = 'created'
                  ORDER BY ts ASC, id ASC LIMIT 1",
                params![project_id, task_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Local time's offset from UTC now, from SQLite's own localtime conversion: the one place
    /// in the dependency tree that already knows the machine's timezone.
    pub fn utc_offset(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT CAST(strftime('%s', 'now', 'localtime') AS INTEGER) - CAST(strftime('%s', 'now') AS INTEGER)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Resolves `since` / `until` as an agent writes them into a range.
    ///
    /// Accepted: a relative age (`30m`, `12h`, `3d`, `2w`, or `3 days`), `now`, `today`,
    /// `yesterday`, a date (`2026-11-02`) or a date and time (`2026-11-02 14:00`), in local
    /// time. Relative ages exist because an agent is unreliable at knowing what "now" is, and
    /// computing an absolute timestamp for "the last three days" is where a range goes wrong.
    ///
    /// A day given as `until` includes that day: "until 2026-11-03" read as that day's first
    /// second would drop the whole of it, the opposite of what anyone means.
    pub fn change_range(&self, since: Option<&str>, until: Option<&str>, now: i64) -> Result<(i64, i64)> {
        let (since_raw, until_raw) = (since, until);
        let until = match until.map(str::trim).filter(|s| !s.is_empty()) {
            // Exclusive bound, so `now` itself is in.
            None => now + 1,
            Some(u) => self.moment("until", u, now, true)?,
        };
        let since = match since.map(str::trim).filter(|s| !s.is_empty()) {
            None => until - ChangeQuery::DEFAULT_WINDOW,
            Some(s) => self.moment("since", s, now, false)?,
        };
        if since >= until {
            // What the agent passed, not the epoch seconds it became: those it cannot check.
            let shown = |v: Option<&str>, dflt: &str| v.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(dflt).to_string();
            return Err(Error::InvalidValue {
                field: "since",
                value: format!("{} (until {})", shown(since_raw, "7 days before until"), shown(until_raw, "now")),
                valid: "a moment before `until`".into(),
            });
        }
        Ok((since, until))
    }

    fn moment(&self, field: &'static str, raw: &str, now: i64, end_of_day: bool) -> Result<i64> {
        let bad = || Error::InvalidValue {
            field,
            value: raw.to_string(),
            valid: "a relative age (30m, 12h, 3d, 2w), now, today, yesterday, \
                    a date (2026-11-02) or a date and time (2026-11-02 14:00)".into(),
        };
        let s = raw.trim().to_ascii_lowercase();
        if s == "now" {
            return Ok(if end_of_day { now + 1 } else { now });
        }
        if let Some(age) = relative_age(&s) {
            return Ok(now - age);
        }
        // Local midnight via SQLite; `end_of_day` moves a day-only `until` to the next one.
        let day_sql = |modifiers: &str| -> Result<i64> {
            let next = if end_of_day { ", '+1 day'" } else { "" };
            let v: Option<i64> = self.conn.query_row(
                &format!("SELECT CAST(strftime('%s', {modifiers}{next}, 'utc') AS INTEGER)"),
                [],
                |r| r.get(0),
            )?;
            v.ok_or_else(bad)
        };
        match s.as_str() {
            "today" => return day_sql("'now', 'localtime', 'start of day'"),
            "yesterday" => return day_sql("'now', 'localtime', 'start of day', '-1 day'"),
            _ => {}
        }
        // Shape-checked first: SQLite also reads a bare number as a Julian day, so "3" (an
        // agent meaning three days) would quietly become a date in 4710 BC.
        let date_only = is_date(&s);
        if !date_only && !is_date_time(&s) {
            return Err(bad());
        }
        let next = if date_only && end_of_day { ", '+1 day'" } else { "" };
        let v: Option<i64> = self.conn.query_row(
            &format!("SELECT CAST(strftime('%s', ?1{next}, 'utc') AS INTEGER)"),
            [s.replace('t', " ")],
            |r| r.get(0),
        )?;
        v.ok_or_else(bad)
    }
}

fn sort_newest_first(rows: &mut [Change]) {
    rows.sort_by(|a, b| {
        let (a, b) = (a.newest(), b.newest());
        b.ts.cmp(&a.ts).then(b.id.cmp(&a.id))
    });
}

/// `3d`, `12h`, `30m`, `2w`, `3 days`, `1 hour` -> seconds. No months: "m" is minutes, and a
/// unit that means two things is how a range ends up off by a factor of 43 000.
fn relative_age(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches(" ago");
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let n: i64 = s[..digits].parse().ok()?;
    let unit = match s[digits..].trim() {
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        "w" | "week" | "weeks" => 7 * 86400,
        _ => return None,
    };
    n.checked_mul(unit)
}

/// `YYYY-MM-DD`
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// `YYYY-MM-DD HH:MM` or `YYYY-MM-DDTHH:MM`, seconds optional.
fn is_date_time(s: &str) -> bool {
    let b = s.as_bytes();
    if !(b.len() == 16 || b.len() == 19) || !is_date(&s[..10]) || !(b[10] == b' ' || b[10] == b't') {
        return false;
    }
    b[11..].iter().enumerate().all(|(i, c)| if i == 2 || i == 5 { *c == b':' } else { c.is_ascii_digit() })
}
