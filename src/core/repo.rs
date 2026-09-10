//! Repos: the local checkouts a board's work happens in, and which tickets touch which.
//!
//! # Why a board owns its repos
//!
//! A body of work -- a customer, a product, a Planio project -- usually spans several
//! repositories, and a ticket in it touches some of them. If every repo were its own board, a
//! ticket touching two of them would have to live on one, and an agent opening the other would
//! never see it. So a board owns repos, and registering one also registers its root as a path
//! alias: an agent opening any of the board's repos lands on the same board.
//!
//! # Why a ticket with no repo is flagged, not blocked
//!
//! `status` is authoritative and carries behaviour -- `is_open`, `board_rank`, what "3 open"
//! means. A status forced by a rule is one the agent did not set and cannot explain, and it
//! would make `blocked` mean two different things. The board says `no repo set` instead, and
//! only on a board that tracks repos at all: on one that does not, every task would say it.
//!
//! # Who registers them
//!
//! A person, through the web UI's repos menu. The agent names repos on tasks and never
//! registers one: registering changes which board a directory resolves to, and that is not a
//! side effect an agent should reach for while filing a side quest.

use crate::core::error::{Error, Result};
use crate::core::model::{Actor, Repo, RepoSummary, Task, TaskLinks};
use crate::core::store::{now, Store};
use crate::core::workstream::{is_missing_schema, normalize_name};
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

const REPO_COLS: &str = "id, project_id, name, path, created_at";

/// `REPO_COLS` with a table alias, derived rather than written out twice -- the `TASK_COLS`
/// trap in CLAUDE.md, avoided the same way `ws_cols` avoids it.
fn repo_cols(alias: &str) -> String {
    REPO_COLS.split(", ").map(|c| format!("{alias}.{c}")).collect::<Vec<_>>().join(", ")
}

fn row_to_repo(r: &rusqlite::Row<'_>) -> rusqlite::Result<Repo> {
    Ok(Repo { id: r.get(0)?, project_id: r.get(1)?, name: r.get(2)?, path: r.get(3)?, created_at: r.get(4)? })
}

/// The read paths below also run from the SessionStart hook, which opens the store read-only
/// and never migrates it. Against a store older than 007 the tables and `tasks.planio` do not
/// exist, and that has to read as "this board tracks no repos" -- which renders exactly the
/// pre-007 board -- rather than as an error that costs the hook its whole output.
fn tolerant<T: Default>(r: rusqlite::Result<T>) -> Result<T> {
    match r {
        Ok(v) => Ok(v),
        Err(e) if is_missing_schema(&e) => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

fn check_name(raw: &str) -> Result<String> {
    let name = normalize_name(raw);
    if name.is_empty() {
        return Err(Error::InvalidValue {
            field: "name",
            value: raw.to_string(),
            valid: "a name with at least one letter or digit".into(),
        });
    }
    Ok(name)
}

impl Store {
    /// Every repo on a board, by name.
    pub fn repos(&self, project_id: i64) -> Result<Vec<Repo>> {
        tolerant((|| {
            let mut st = self.conn.prepare(&format!(
                "SELECT {REPO_COLS} FROM repos WHERE project_id = ?1 ORDER BY name, id"
            ))?;
            let rows = st.query_map([project_id], row_to_repo)?.collect::<rusqlite::Result<Vec<_>>>();
            rows
        })())
    }

    pub fn repo_count(&self, project_id: i64) -> Result<usize> {
        tolerant(self.conn.query_row(
            "SELECT COUNT(*) FROM repos WHERE project_id = ?1",
            [project_id],
            |r| r.get::<_, i64>(0),
        ).map(|n| n as usize))
    }

    /// Scoped by project like every other lookup: the store is global, so a bare id could name
    /// a repo another board owns.
    pub fn repo(&self, project_id: i64, id: i64) -> Result<Repo> {
        let found = self.conn.query_row(
            &format!("SELECT {REPO_COLS} FROM repos WHERE id = ?1 AND project_id = ?2"),
            params![id, project_id],
            row_to_repo,
        ).optional()?;
        match found {
            Some(r) => Ok(r),
            None => Err(self.unknown_repo(project_id, &format!("#{id}"))?),
        }
    }

    /// The not-found error, carrying the names that do exist so the caller can correct itself.
    /// An empty board says where repos come from instead, since an agent cannot add one.
    fn unknown_repo(&self, project_id: i64, value: &str) -> Result<Error> {
        let names: Vec<String> = self.repos(project_id)?.into_iter().map(|r| r.name).collect();
        Ok(Error::InvalidValue {
            field: "repo",
            value: value.to_string(),
            valid: if names.is_empty() {
                "none yet -- a person registers this board's repos in the web UI's repos menu \
                 (ai-kanban serve)".into()
            } else {
                names.join(", ")
            },
        })
    }

    /// The repos menu: every repo with how many tickets touch it.
    ///
    /// Unlike the agent's workstream directory this lists repos with no tickets too -- a repo
    /// registered a moment ago has none, and a menu that hid it would be one nobody could
    /// pick from.
    pub fn repo_summaries(&self, project_id: i64) -> Result<Vec<RepoSummary>> {
        let sql = format!(
            "SELECT {}, COUNT(t.id),
                    COALESCE(SUM(CASE WHEN t.status IN ('backlog','doing','blocked') THEN 1 ELSE 0 END), 0)
               FROM repos r
               LEFT JOIN task_repos tr ON tr.repo_id = r.id
               LEFT JOIN tasks t ON t.id = tr.task_id
              WHERE r.project_id = ?1
              GROUP BY r.id
              ORDER BY r.name, r.id",
            repo_cols("r"),
        );
        tolerant((|| {
            let mut st = self.conn.prepare(&sql)?;
            let rows = st.query_map([project_id], |r| Ok(RepoSummary {
                repo: row_to_repo(r)?,
                total: r.get::<_, i64>(5)? as usize,
                open: r.get::<_, i64>(6)? as usize,
            }))?.collect::<rusqlite::Result<Vec<_>>>();
            rows
        })())
    }

    /// Registers a local checkout on this board, and claims its directory for the board.
    ///
    /// Registering the same path twice returns the existing repo rather than failing: the
    /// person asked for a state that already holds.
    ///
    /// Refused when **another** board already claims the directory or anything below it.
    /// `project_paths` maps one directory to exactly one board, and resolution checks the
    /// deepest alias first -- so a subdirectory the other board learned would keep answering
    /// for it, and this registration would silently not take effect. Taking the paths over
    /// instead would move that board's future sessions out from under its history. When both
    /// are the same work, `merge` is the repair, and the error says so.
    pub fn add_repo(&self, project_id: i64, path: &Path, name: Option<&str>, actor: Actor) -> Result<Repo> {
        let bad_path = || Error::InvalidValue {
            field: "path",
            value: path.to_string_lossy().into_owned(),
            valid: "an existing directory on this machine".into(),
        };
        // Canonical, so /tmp and /private/tmp -- or a symlinked checkout -- are one repo, and
        // so the alias matches what `resolve_project` will look up.
        let canon = std::fs::canonicalize(path).map_err(|_| bad_path())?;
        if !canon.is_dir() {
            return Err(bad_path());
        }
        let path_s = canon.to_string_lossy().into_owned();

        let existing = self.conn.query_row(
            &format!("SELECT {REPO_COLS} FROM repos WHERE path = ?1"),
            [&path_s],
            row_to_repo,
        ).optional()?;
        if let Some(r) = existing {
            if r.project_id == project_id {
                return Ok(r);
            }
            let other = self.project(r.project_id)?;
            return Err(Error::PathClaimed { path: path_s, board: other.name, key: other.key });
        }

        // `substr` rather than LIKE: LIKE is case-insensitive for ASCII and treats `_` and `%`
        // in a path as wildcards, so it would match directories that are not below this one.
        let claimed: Option<i64> = self.conn.query_row(
            "SELECT project_id FROM project_paths
              WHERE project_id != ?2
                AND (path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')
              LIMIT 1",
            params![path_s, project_id],
            |r| r.get(0),
        ).optional()?;
        if let Some(other) = claimed {
            let other = self.project(other)?;
            return Err(Error::PathClaimed { path: path_s, board: other.name, key: other.key });
        }

        let fallback = canon.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path_s.clone());
        let name = check_name(name.filter(|n| !n.trim().is_empty()).unwrap_or(&fallback))?;
        self.ensure_repo_name_free(project_id, &name, None)?;

        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "INSERT INTO repos (project_id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, name, path_s, now()],
        )?;
        let id = self.conn.last_insert_rowid();
        // The half that makes an agent opening this repo land on this board.
        self.add_path_alias(project_id, &canon)?;
        // Not housekeeping: "eee-web joined this board" is history a cold agent can use, and
        // it is the entry that explains why a directory started resolving here.
        self.write_event(project_id, None, actor, "repo_added", &format!("{name} ({path_s})"))?;
        tx.commit()?;
        self.repo(project_id, id)
    }

    pub fn rename_repo(&self, project_id: i64, id: i64, name: &str, actor: Actor) -> Result<Repo> {
        let before = self.repo(project_id, id)?;
        let name = check_name(name)?;
        if name == before.name {
            return Ok(before);
        }
        self.ensure_repo_name_free(project_id, &name, Some(id))?;
        self.conn.execute(
            "UPDATE repos SET name = ?3 WHERE id = ?1 AND project_id = ?2",
            params![id, project_id, name],
        )?;
        self.write_event(project_id, None, actor, "repo_renamed", &format!("{} -> {name}", before.name))?;
        self.repo(project_id, id)
    }

    /// Takes a repo off the board and off every ticket that named it. Returns how many
    /// tickets lost it, which the event records.
    ///
    /// The directory's path alias is **left in place**. Removing a repo from the menu says it
    /// is no longer part of this work; it does not say its history belongs somewhere else.
    /// Dropping the alias would silently change which board that directory resolves to next
    /// session -- to a brand-new empty one -- which is the split this store is built to
    /// prevent.
    pub fn remove_repo(&self, project_id: i64, id: i64, actor: Actor) -> Result<usize> {
        let r = self.repo(project_id, id)?;
        let tx = self.conn.unchecked_transaction()?;
        // Explicit rather than left to ON DELETE CASCADE, for the reason `forget.rs` gives:
        // `foreign_keys` is per-connection state, and a link left behind here would keep a
        // ticket pointing at a repo that no longer exists.
        let unlinked = self.conn.execute("DELETE FROM task_repos WHERE repo_id = ?1", [id])?;
        self.conn.execute("DELETE FROM repos WHERE id = ?1 AND project_id = ?2", params![id, project_id])?;
        self.write_event(
            project_id, None, actor, "repo_removed",
            &format!("{} ({}), was on {unlinked} ticket{}", r.name, r.path, if unlinked == 1 { "" } else { "s" }),
        )?;
        tx.commit()?;
        Ok(unlinked)
    }

    fn ensure_repo_name_free(&self, project_id: i64, name: &str, except: Option<i64>) -> Result<()> {
        let taken: Option<i64> = self.conn.query_row(
            "SELECT id FROM repos WHERE project_id = ?1 AND name = ?2",
            params![project_id, name],
            |r| r.get(0),
        ).optional()?;
        match taken {
            Some(id) if Some(id) != except => Err(Error::InvalidValue {
                field: "name",
                value: name.to_string(),
                valid: "a name no other repo on this board already has".into(),
            }),
            _ => Ok(()),
        }
    }

    /// Repo references as an agent or a person typed them -- names, in any spelling the
    /// normalizer folds, or paths -- turned into ids on this board.
    ///
    /// An unknown one fails with the names that do exist rather than being skipped: a ticket
    /// silently missing one of its repos is an agent that never opens that checkout.
    pub fn resolve_repos(&self, project_id: i64, refs: &[String]) -> Result<Vec<i64>> {
        let repos = self.repos(project_id)?;
        let mut out = Vec::new();
        for raw in refs.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let name = normalize_name(raw);
            let path = std::fs::canonicalize(raw).ok().map(|p| p.to_string_lossy().into_owned());
            match repos.iter().find(|r| r.name == name || path.as_deref() == Some(r.path.as_str())) {
                Some(r) if !out.contains(&r.id) => out.push(r.id),
                Some(_) => {}
                None => return Err(self.unknown_repo(project_id, raw)?),
            }
        }
        Ok(out)
    }

    /// Every id must be a repo on this board -- the same-board guard `blocked_by` and
    /// workstreams have, for the same reason.
    pub(crate) fn check_repos(&self, project_id: i64, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.repo(project_id, *id)?;
        }
        Ok(())
    }

    /// Replaces a task's repos. Callers check ownership first with `check_repos`.
    pub(crate) fn set_task_repos(&self, task_id: i64, ids: &[i64]) -> Result<()> {
        self.conn.execute("DELETE FROM task_repos WHERE task_id = ?1", [task_id])?;
        for id in ids {
            self.conn.execute(
                "INSERT OR IGNORE INTO task_repos (task_id, repo_id) VALUES (?1, ?2)",
                params![task_id, id],
            )?;
        }
        Ok(())
    }

    /// One task's repos, with their paths.
    pub fn task_repos(&self, project_id: i64, task_id: i64) -> Result<Vec<Repo>> {
        let sql = format!(
            "SELECT {} FROM task_repos tr JOIN repos r ON r.id = tr.repo_id
              WHERE tr.task_id = ?1 AND r.project_id = ?2
              ORDER BY r.name, r.id",
            repo_cols("r"),
        );
        tolerant((|| {
            let mut st = self.conn.prepare(&sql)?;
            let rows = st.query_map(params![task_id, project_id], row_to_repo)?.collect::<rusqlite::Result<Vec<_>>>();
            rows
        })())
    }

    /// A task's Planio ticket. A lookup of its own because `planio` is out of `TASK_COLS`.
    pub fn task_planio(&self, project_id: i64, task_id: i64) -> Result<Option<i64>> {
        tolerant(self.conn.query_row(
            "SELECT planio FROM tasks WHERE id = ?1 AND project_id = ?2",
            params![task_id, project_id],
            |r| r.get::<_, Option<i64>>(0),
        ).optional().map(Option::flatten))
    }

    /// Repos and Planio refs for a set of listed tasks, in two queries however many rows.
    ///
    /// Only tasks that have either appear. Batched for the reason `tags_for` is: the board
    /// lists a few dozen rows, and a per-row lookup is the kind of thing that stops being free
    /// the moment someone raises the cap.
    pub fn links_for(&self, project_id: i64, tasks: &[Task]) -> Result<Vec<TaskLinks>> {
        if tasks.is_empty() {
            return Ok(vec![]);
        }
        let holes = tasks.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let ids: Vec<i64> = std::iter::once(project_id).chain(tasks.iter().map(|t| t.id)).collect();
        type Found = (HashMap<i64, i64>, HashMap<i64, Vec<String>>);
        let found = (|| -> rusqlite::Result<Found> {
            let mut st = self.conn.prepare(&format!(
                "SELECT id, planio FROM tasks WHERE project_id = ? AND id IN ({holes}) AND planio IS NOT NULL"
            ))?;
            let planio = st
                .query_map(rusqlite::params_from_iter(&ids), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?;
            let mut st = self.conn.prepare(&format!(
                "SELECT tr.task_id, r.name FROM task_repos tr JOIN repos r ON r.id = tr.repo_id
                  WHERE r.project_id = ? AND tr.task_id IN ({holes})
                  ORDER BY r.name, r.id"
            ))?;
            let mut repos: HashMap<i64, Vec<String>> = HashMap::new();
            for row in st.query_map(rusqlite::params_from_iter(&ids), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (task, name) = row?;
                repos.entry(task).or_default().push(name);
            }
            Ok((planio, repos))
        })();
        let (planio, mut repos) = tolerant(found)?;
        Ok(tasks.iter().filter_map(|t| {
            let r = repos.remove(&t.id).unwrap_or_default();
            let p = planio.get(&t.id).copied();
            (!r.is_empty() || p.is_some()).then(|| TaskLinks { task_id: t.id, repos: r, planio: p })
        }).collect())
    }
}
