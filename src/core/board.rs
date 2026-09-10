//! Board assembly -- the standard response shape.
//!
//! # Why this file is mostly about *limits*
//!
//! The obvious implementation returns every task. That is correct for a ten-task project
//! and wrong for a year-old one with two hundred, and the failure it causes is not "the
//! response is long" -- it is that the board becomes expensive, and cost is precisely what
//! a shortcutting agent optimises away. An agent that stops calling the board is the exact
//! failure this project exists to fix, so response volume is a design constraint here, not
//! a formatting detail.
//!
//! Nothing is dropped silently: whatever the cap excludes is reported as a count.

use crate::core::error::Result;
use crate::core::model::*;
use crate::core::store::{now, Store};
// One column list, shared with the task layer. It was duplicated here, which is a quiet
// trap: both files hand rows to the same `row_to_task`, so a column added to one list and
// not the other shifts every index after it and reads the wrong field into the wrong place.
use crate::core::task::{row_to_task, TASK_COLS};

impl Store {
    /// The snapshot. Always carries its project, so the agent never has to guess which
    /// board it is looking at -- the same reason the header leads with it.
    pub fn board(&self, project_id: i64, q: &BoardQuery) -> Result<BoardSnapshot> {
        let project = self.project(project_id)?;
        // Counts follow the scope rather than the project. When the board is scoped, every
        // number in the response then means one thing -- "3 open" is the scope's three, and
        // the other workstreams carry their own counts in the directory. Mixing
        // project-wide totals with a scoped task list is how "+105 backlog" ends up
        // describing work the reader cannot see and did not ask about.
        let counts = self.status_counts_in(project_id, q.workstream)?;

        // Empty filter means "the open ones". `done` and `archived` are the bulk of an old
        // project and almost never what "where do things stand" means.
        let wanted: Vec<Status> = if q.status.is_empty() {
            Status::ALL.iter().copied().filter(|s| s.is_open()).collect()
        } else {
            q.status.clone()
        };

        let tasks = self.tasks_in(project_id, &wanted, q.limit, q.workstream)?;
        let omitted = compute_omitted(&tasks, &counts, &wanted);

        let workstream = match q.workstream {
            Some(id) => Some(self.workstream(project_id, id)?),
            None => None,
        };
        // Before `tasks` is moved into the snapshot.
        let blocker_status = self.blocker_status(project_id, &tasks)?;
        let links = self.links_for(project_id, &tasks)?;

        Ok(BoardSnapshot {
            project,
            tasks,
            omitted,
            counts,
            recent: self.recent_events_in(project_id, q.recent_limit, q.workstream)?,
            highlight: None,
            // What the scope excluded, stated rather than hidden -- the same rule `omitted`
            // follows for statuses. Computed even when unscoped, so a board with several
            // workstreams and no active one still tells the agent they exist.
            other_workstreams: self.workstream_summaries(project_id, q.workstream)?,
            blocker_status,
            links,
            repo_count: self.repo_count(project_id)?,
            workstream,
            now: now(),
        })
    }

    /// The status of each distinct task the listed tasks are blocked by.
    ///
    /// One query for the whole board rather than one per task: a board is capped at a few
    /// dozen rows, but a per-row lookup is the kind of thing that stops being free the
    /// moment someone raises the cap.
    ///
    /// Scoped by `project_id` like every other task lookup here. The store is global, so a
    /// bare id can name another board's row -- and a blocker id copied across boards would
    /// report a status belonging to work the reader cannot see.
    pub fn blocker_status(&self, project_id: i64, tasks: &[Task]) -> Result<Vec<(i64, Status)>> {
        let mut ids: Vec<i64> = tasks.iter().filter_map(|t| t.blocked_by).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let holes = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let mut st = self.conn.prepare(&format!(
            "SELECT id, status FROM tasks WHERE project_id = ? AND id IN ({holes})"
        ))?;
        let params = std::iter::once(project_id).chain(ids.into_iter()).collect::<Vec<_>>();
        let rows = st.query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Status>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The response after a write. Same shape, tighter caps, and the changed task marked.
    ///
    /// It is a *bounded* board rather than a bare acknowledgement because "did it land"
    /// should never need a second call -- and a bare "ok" would guarantee one.
    ///
    /// It shows **in-flight work only** (`doing`, `blocked`) plus the task that changed.
    /// The obvious alternative -- the same open-task list a `board` call returns -- was
    /// measured on a realistic board and spent most of its budget listing a dozen unrelated
    /// backlog items. That is noise the agent pays for on every single `task_add`, and the
    /// backlog slice does not even answer the question a write response should ("did it
    /// land, and what is in flight"). The backlog is still accounted for, as a count.
    pub fn board_after_mutation(&self, project_id: i64, highlight: i64) -> Result<BoardSnapshot> {
        let mut q = BoardQuery::after_mutation();
        // The write response inherits the board's current scope. Without this, filing a
        // task inside a workstream would answer with an unscoped board -- the noise the
        // scope exists to remove, arriving on the one response the agent pays for on every
        // single `task_add`.
        q.workstream = self.current_workstream(project_id)?.map(|w| w.id);
        // In-flight only. Note this stays narrow even when the changed task is a backlog
        // item: widening the filter to include its status would pull the entire backlog
        // back in and undo the narrowing. The task itself is guaranteed to appear via the
        // splice below, which is the mechanism that makes the narrow filter safe.
        q.status = vec![Status::Doing, Status::Blocked];
        let mut snap = self.board(project_id, &q)?;
        // Report every open status that is not being listed, so "+29 backlog" appears even
        // though backlog was filtered out. Silently omitting a whole status would make the
        // response look like the board is smaller than it is.
        let unlisted: Vec<Status> = Status::ALL.iter().copied()
            .filter(|s| s.is_open() && !q.status.contains(s))
            .collect();
        snap.omitted.extend(compute_omitted(&snap.tasks, &snap.counts, &unlisted));
        snap.highlight = Some(highlight);

        // Guarantee the highlighted task is present even if the cap or the filter excluded
        // it -- confirming the write is this response's entire job. It *swaps out* the
        // least relevant row rather than appending, because exceeding the cap here would
        // undermine the one guarantee this response makes about its own size.
        if !snap.tasks.iter().any(|t| t.id == highlight) {
            if let Some(t) = self.task_opt(project_id, highlight)? {
                if snap.tasks.len() >= q.limit {
                    snap.tasks.pop();
                }
                snap.tasks.push(t);
                sort_board(&mut snap.tasks);
                // Both describe the listed rows, and the listed rows just changed. Left as
                // computed, the task this response exists to confirm would be the one row
                // missing its repos, its Planio ref and its blocker's status.
                snap.links = self.links_for(project_id, &snap.tasks)?;
                snap.blocker_status = self.blocker_status(project_id, &snap.tasks)?;
                // The swap changed what is shown, so the "and N more" figures have to be
                // recomputed or they would describe the pre-swap list.
                let mut wanted = q.status.clone();
                let extra: Vec<Status> = Status::ALL.iter().copied()
                    .filter(|s| s.is_open() && !wanted.contains(s)).collect();
                wanted.extend(extra);
                snap.omitted = compute_omitted(&snap.tasks, &snap.counts, &wanted);
            }
        }
        Ok(snap)
    }

    fn tasks_in(&self, project_id: i64, statuses: &[Status], limit: usize, workstream: Option<i64>) -> Result<Vec<Task>> {
        if statuses.is_empty() {
            return Ok(vec![]);
        }
        // Ordering is done in SQL so the LIMIT keeps the *most relevant* tasks rather than
        // an arbitrary slice: active workstream first, then in-flight work, then urgency,
        // then recency.
        //
        // The workstream term has to come FIRST, ahead of status, and that is the whole
        // point of the feature rather than a detail. Every board that predates workstreams
        // has tasks with `workstream_id IS NULL`, and those stay visible (see
        // `scope_filter`). Ordered by status alone, fifty unscoped legacy tickets crowd the
        // three tasks in the active workstream straight out of a 30-row limit -- shipping
        // the exact failure measured in note #16, with a new column that was supposed to
        // fix it.
        //
        // The cost, stated because it is a real trade: an unscoped `doing` task now sorts
        // below an active-workstream `backlog` one. That is the intended reading -- if the
        // board is scoped to the contact form, the contact-form backlog is what "where do
        // things stand" means -- and the displaced work is never dropped, only counted.
        let placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let limit_pos = statuses.len() + 3;
        let sql = format!(
            "SELECT {TASK_COLS} FROM tasks
              WHERE project_id = ?1 AND {} AND status IN ({placeholders})
              ORDER BY {}
                       CASE status
                         WHEN 'doing' THEN 0 WHEN 'blocked' THEN 1 WHEN 'backlog' THEN 2
                         WHEN 'done' THEN 3 ELSE 4 END,
                       CASE priority
                         WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END,
                       updated_at DESC,
                       id DESC
              LIMIT ?{limit_pos}",
            scope_filter(workstream),
            scope_order(workstream),
        );
        let mut st = self.conn.prepare(&sql)?;
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id), Box::new(workstream)];
        for s in statuses { p.push(Box::new(s.as_str())); }
        p.push(Box::new(limit as i64));
        let refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        Ok(st.query_map(refs.as_slice(), row_to_task)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn status_counts(&self, project_id: i64) -> Result<Vec<(Status, usize)>> {
        self.status_counts_in(project_id, None)
    }

    /// `status_counts` restricted to a workstream. Shares `scope_filter` with `tasks_in`
    /// so the listed rows and the counts describing them can never disagree about what the
    /// scope contains -- a board whose header contradicts its own list is worse than one
    /// with no header.
    pub fn status_counts_in(&self, project_id: i64, workstream: Option<i64>) -> Result<Vec<(Status, usize)>> {
        let sql = format!(
            "SELECT status, COUNT(*) FROM tasks WHERE project_id = ?1 AND {} GROUP BY status",
            scope_filter(workstream),
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params![project_id, workstream],
            |r| Ok((r.get::<_, Status>(0)?, r.get::<_, i64>(1)? as usize)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out: Vec<(Status, usize)> = rows;
        out.sort_by_key(|(s, _)| s.board_rank());
        Ok(out)
    }

    /// Counts across every board, for the person's All Projects view.
    pub fn status_counts_all(&self) -> Result<Vec<(Status, usize)>> {
        let mut st = self.conn.prepare("SELECT status, COUNT(*) FROM tasks GROUP BY status")?;
        let mut out = st.query_map([], |r| Ok((r.get::<_, Status>(0)?, r.get::<_, i64>(1)? as usize)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.sort_by_key(|(s, _)| s.board_rank());
        Ok(out)
    }

    /// The number of boards listed by `board(project: "all")`.
    ///
    /// Capped for the same reason everything else is, but this one is worse: its cost
    /// scales with how many repositories the user has touched all year, which is not a
    /// number this project controls.
    pub const SUMMARY_LIMIT: usize = 25;

    /// `board(project: "all")`.
    ///
    /// Per-project summaries, never a merged task list: every open task across every
    /// project is not a board, it is a pile. Digging across projects is `recall`'s job.
    ///
    /// Returns the summaries and the total number of boards, so the response can report
    /// what the cap left out instead of implying the user has fewer boards than they do.
    pub fn project_summaries(&self, limit: usize) -> Result<(Vec<ProjectSummary>, usize)> {
        let mut out = Vec::new();
        for project in self.all_projects()? {
            let counts = self.status_counts(project.id)?;
            let open = counts.iter().filter(|(s, _)| s.is_open()).map(|(_, n)| n).sum();
            out.push(ProjectSummary {
                // Unscoped on purpose: this is the every-board summary, and narrowing it
                // to one board's active workstream would hide in-flight work on the very
                // view whose job is "what was I working on, anywhere".
                doing: self.tasks_in(project.id, &[Status::Doing], 5, None)?,
                last_activity: self.last_activity(project.id)?,
                open,
                project,
            });
        }
        // Most recently touched first -- "what was I working on" is the question this
        // answers, and it is also what makes the cap safe: the boards that fall off the end
        // are the ones untouched the longest.
        out.sort_by_key(|s| std::cmp::Reverse(s.last_activity.unwrap_or(0)));
        let total = out.len();
        out.truncate(limit);
        Ok((out, total))
    }
}

/// The one definition of "in scope", shared by the task listing and the counts that
/// describe it. Expects `?2` to be the workstream id.
///
/// Unscoped tasks (`workstream_id IS NULL`) are in scope **always**, and that is deliberate
/// rather than a concession to legacy data. A task with no workstream is general project
/// work -- it belongs to every view, not to none. It also means turning this feature on
/// hides nothing on day one: every board that predates 005 has entirely unscoped tasks,
/// and scoping such a board strictly would blank it.
///
/// # Why this returns nothing at all when unscoped
///
/// It would be shorter to always emit `(?2 IS NULL OR workstream_id = ?2 OR ...)` and let a
/// NULL parameter make it true. That version is **broken on a store this binary has not
/// migrated**: naming `workstream_id` in a WHERE clause requires the column to exist, and a
/// pre-005 store does not have it. The query fails, and since every hook path is `.ok()?`,
/// the SessionStart board silently disappears.
///
/// That is the exact regression migration 005 went out of its way to avoid by keeping the
/// column out of `TASK_COLS` and `MIN_READABLE_VERSION` at 2 -- and it was reintroduced
/// here, in a WHERE clause, because "not in the SELECT list" was mistaken for "not
/// referenced". Naming the column anywhere is enough. So when nothing is scoped, the SQL
/// must not mention it at all.
/// `?2` stays referenced in BOTH arms, deliberately. The unscoped arm is `?2 IS NULL`,
/// which is true (the caller binds NULL) and names no column -- so the parameter numbering
/// downstream is identical either way. Returning a bare `1` instead drops the only use of
/// `?2` and SQLite then reports the statement as taking one parameter while the caller
/// binds two.
fn scope_filter(workstream: Option<i64>) -> &'static str {
    match workstream {
        Some(_) => "(workstream_id = ?2 OR workstream_id IS NULL)",
        None => "(?2 IS NULL)",
    }
}

/// The relevance term that puts the active workstream first, or nothing when unscoped.
/// Same reasoning as `scope_filter`: an ORDER BY naming a missing column fails too.
fn scope_order(workstream: Option<i64>) -> &'static str {
    match workstream {
        Some(_) => "CASE WHEN workstream_id = ?2 THEN 0 ELSE 1 END,",
        None => "",
    }
}

/// What the cap left out, per status. Reported rather than dropped: a board that quietly
/// hides work is worse than one that admits it is showing a slice.
fn compute_omitted(tasks: &[Task], counts: &[(Status, usize)], wanted: &[Status]) -> Vec<(Status, usize)> {
    let mut omitted = Vec::new();
    for s in wanted {
        let shown = tasks.iter().filter(|t| t.status == *s).count();
        let total = counts.iter().find(|(k, _)| k == s).map(|(_, n)| *n).unwrap_or(0);
        if total > shown {
            omitted.push((*s, total - shown));
        }
    }
    omitted
}

pub fn sort_board(tasks: &mut [Task]) {
    tasks.sort_by_key(|t| (t.status.board_rank(), t.priority.rank(), std::cmp::Reverse(t.updated_at)));
}
