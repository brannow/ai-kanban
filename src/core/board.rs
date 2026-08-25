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
use crate::core::task::row_to_task;

const TASK_COLS: &str = "id, project_id, title, body, status, type, origin, priority, blocked_by, created_at, updated_at";

impl Store {
    /// The snapshot. Always carries its project, so the agent never has to guess which
    /// board it is looking at -- the same reason the header leads with it.
    pub fn board(&self, project_id: i64, q: &BoardQuery) -> Result<BoardSnapshot> {
        let project = self.project(project_id)?;
        let counts = self.status_counts(project_id)?;

        // Empty filter means "the open ones". `done` and `archived` are the bulk of an old
        // project and almost never what "where do things stand" means.
        let wanted: Vec<Status> = if q.status.is_empty() {
            Status::ALL.iter().copied().filter(|s| s.is_open()).collect()
        } else {
            q.status.clone()
        };

        let tasks = self.tasks_in(project_id, &wanted, q.limit)?;
        let omitted = compute_omitted(&tasks, &counts, &wanted);

        Ok(BoardSnapshot {
            project,
            tasks,
            omitted,
            counts,
            recent: self.recent_events(project_id, q.recent_limit)?,
            highlight: None,
            now: now(),
        })
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

    fn tasks_in(&self, project_id: i64, statuses: &[Status], limit: usize) -> Result<Vec<Task>> {
        if statuses.is_empty() {
            return Ok(vec![]);
        }
        // Ordering is done in SQL so the LIMIT keeps the *most relevant* tasks rather than
        // an arbitrary slice: in-flight work first, then urgency, then recency.
        let placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {TASK_COLS} FROM tasks
              WHERE project_id = ?1 AND status IN ({placeholders})
              ORDER BY CASE status
                         WHEN 'doing' THEN 0 WHEN 'blocked' THEN 1 WHEN 'backlog' THEN 2
                         WHEN 'done' THEN 3 ELSE 4 END,
                       CASE priority
                         WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END,
                       updated_at DESC,
                       id DESC
              LIMIT ?{}",
            statuses.len() + 2
        );
        let mut st = self.conn.prepare(&sql)?;
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project_id)];
        for s in statuses { p.push(Box::new(s.as_str())); }
        p.push(Box::new(limit as i64));
        let refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        Ok(st.query_map(refs.as_slice(), row_to_task)?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn status_counts(&self, project_id: i64) -> Result<Vec<(Status, usize)>> {
        let mut st = self.conn.prepare(
            "SELECT status, COUNT(*) FROM tasks WHERE project_id = ?1 GROUP BY status",
        )?;
        let rows = st.query_map([project_id], |r| Ok((r.get::<_, Status>(0)?, r.get::<_, i64>(1)? as usize)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out: Vec<(Status, usize)> = rows;
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
                doing: self.tasks_in(project.id, &[Status::Doing], 5)?,
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
