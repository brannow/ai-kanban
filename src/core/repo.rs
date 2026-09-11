//! Repos: the local checkouts boards' work happens in, and which tickets touch which.
//!
//! # Boards own repos, and a repo can serve several boards
//!
//! A board is a body of work -- a customer, a product, a Planio project -- and it usually
//! spans several repositories. A repository can serve more than one of them, too: a shared
//! library, a monorepo two projects deploy from. So repos are global and tied to boards
//! through `board_repos`, many-to-many, and a ticket can name any repo its board has.
//!
//! # Exactly one home
//!
//! What cannot be shared is where a folder resolves. An agent opening a directory has to land
//! on one board, the same one every time, or the folder reads a different memory on different
//! days -- the split this store exists to prevent. So every repo has a HOME board: the board
//! its path alias points at. It is always one of the repo's boards. It starts as the board
//! the repo was first added to, a person can move it, and when the home lets the repo go it
//! passes to the earliest remaining board rather than leaving the folder pointing nowhere.
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

const REPO_COLS: &str = "id, home_project_id, name, path, created_at";

/// `REPO_COLS` with a table alias, derived rather than written out twice -- the `TASK_COLS`
/// trap in CLAUDE.md, avoided the same way `ws_cols` avoids it.
fn repo_cols(alias: &str) -> String {
    REPO_COLS.split(", ").map(|c| format!("{alias}.{c}")).collect::<Vec<_>>().join(", ")
}

fn row_to_repo(r: &rusqlite::Row<'_>) -> rusqlite::Result<Repo> {
    Ok(Repo { id: r.get(0)?, home_project_id: r.get(1)?, name: r.get(2)?, path: r.get(3)?, created_at: r.get(4)? })
}

/// "This path, or anything below it", with the path bound as `?1`. `substr` rather than LIKE:
/// LIKE is case-insensitive for ASCII and treats `_` and `%` in a path as wildcards, so it
/// would match directories that are not below this one.
const AT_OR_BELOW: &str = "(path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')";

/// The read paths below also run from the SessionStart hook, which opens the store read-only
/// and never migrates it. Against a store older than 007 or 008 the tables or columns they
/// name do not exist, and that has to read as "this board tracks no repos" -- the board as it
/// rendered before -- rather than as an error that costs the hook its whole output.
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
        let sql = format!(
            "SELECT {} FROM board_repos br JOIN repos r ON r.id = br.repo_id
              WHERE br.project_id = ?1 ORDER BY r.name, r.id",
            repo_cols("r"),
        );
        tolerant((|| {
            let mut st = self.conn.prepare(&sql)?;
            let rows = st.query_map([project_id], row_to_repo)?.collect::<rusqlite::Result<Vec<_>>>();
            rows
        })())
    }

    /// Every repo in the store, whichever boards it is on -- what a board can attach.
    pub fn all_repos(&self) -> Result<Vec<Repo>> {
        tolerant((|| {
            let mut st = self.conn.prepare(&format!("SELECT {REPO_COLS} FROM repos ORDER BY name, id"))?;
            let rows = st.query_map([], row_to_repo)?.collect::<rusqlite::Result<Vec<_>>>();
            rows
        })())
    }

    pub fn repo_count(&self, project_id: i64) -> Result<usize> {
        tolerant(self.conn.query_row(
            "SELECT COUNT(*) FROM board_repos WHERE project_id = ?1",
            [project_id],
            |r| r.get::<_, i64>(0),
        ).map(|n| n as usize))
    }

    /// A repo **on this board**. Scoped like every other lookup: the store is global, and a
    /// bare id would let a ticket name a checkout its board does not have.
    pub fn repo(&self, project_id: i64, id: i64) -> Result<Repo> {
        let found = self.conn.query_row(
            &format!(
                "SELECT {} FROM board_repos br JOIN repos r ON r.id = br.repo_id
                  WHERE r.id = ?1 AND br.project_id = ?2",
                repo_cols("r"),
            ),
            params![id, project_id],
            row_to_repo,
        ).optional()?;
        match found {
            Some(r) => Ok(r),
            None => Err(self.unknown_repo(project_id, &format!("#{id}"))?),
        }
    }

    fn repo_by_path(&self, path: &str) -> Result<Option<Repo>> {
        Ok(self.conn.query_row(
            &format!("SELECT {REPO_COLS} FROM repos WHERE path = ?1"),
            [path],
            row_to_repo,
        ).optional()?)
    }

    /// The not-found error, carrying the names that do exist so the caller can correct itself.
    /// An empty board says where repos come from instead, since an agent cannot add one.
    fn unknown_repo(&self, project_id: i64, value: &str) -> Result<Error> {
        let names: Vec<String> = self.repos(project_id)?.into_iter().map(|r| r.name).collect();
        Ok(Error::InvalidValue {
            field: "repo",
            value: value.to_string(),
            valid: if names.is_empty() {
                "none yet -- a person adds this board's repos in the web UI's repos menu \
                 (ai-kanban serve)".into()
            } else {
                names.join(", ")
            },
        })
    }

    /// A board's repos menu: every repo on it, how many of this board's tickets touch it, and
    /// where its folder opens.
    ///
    /// Repos with no tickets are listed too -- one added a moment ago has none, and a menu that
    /// hid it would be one nobody could pick from.
    pub fn repo_summaries(&self, project_id: i64) -> Result<Vec<RepoSummary>> {
        let sql = format!(
            "SELECT {}, COUNT(t.id),
                    COALESCE(SUM(CASE WHEN t.status IN ('backlog','doing','blocked') THEN 1 ELSE 0 END), 0),
                    home.name
               FROM board_repos br
               JOIN repos r ON r.id = br.repo_id
               JOIN projects home ON home.id = r.home_project_id
               LEFT JOIN task_repos tr ON tr.repo_id = r.id
               LEFT JOIN tasks t ON t.id = tr.task_id AND t.project_id = br.project_id
              WHERE br.project_id = ?1
              GROUP BY r.id
              ORDER BY r.name, r.id",
            repo_cols("r"),
        );
        tolerant((|| {
            let mut st = self.conn.prepare(&sql)?;
            let mut rows = st.query_map([project_id], |r| Ok(RepoSummary {
                repo: row_to_repo(r)?,
                total: r.get::<_, i64>(5)? as usize,
                open: r.get::<_, i64>(6)? as usize,
                home_board: r.get(7)?,
                other_boards: vec![],
            }))?.collect::<rusqlite::Result<Vec<_>>>()?;

            let mut st = self.conn.prepare(
                "SELECT br.repo_id, p.name FROM board_repos br JOIN projects p ON p.id = br.project_id
                  WHERE br.project_id != ?1
                    AND br.repo_id IN (SELECT repo_id FROM board_repos WHERE project_id = ?1)
                  ORDER BY p.name, p.id",
            )?;
            let mut others: HashMap<i64, Vec<String>> = HashMap::new();
            for row in st.query_map([project_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (repo, board) = row?;
                others.entry(repo).or_default().push(board);
            }
            for s in &mut rows {
                s.other_boards = others.remove(&s.repo.id).unwrap_or_default();
            }
            Ok(rows)
        })())
    }

    /// Puts a local checkout on this board.
    ///
    /// A checkout that is **already a repo** -- on another board -- is attached, and its home
    /// stays where it is: sharing a repo does not change where its folder opens. Moving that
    /// is `set_repo_home`, a step of its own that says what it did. Adding one this board
    /// already has returns it unchanged: the person asked for a state that already holds.
    ///
    /// A checkout that is **new** makes this board its home and claims its directory. That is
    /// refused when another board already claims the directory or anything below it: resolution
    /// checks the deepest alias first, so that board's subdirectory would keep answering and
    /// this registration would silently not take effect -- and taking its paths over instead
    /// would move its future sessions out from under its history. When both are the same work,
    /// `merge` is the repair, and the error says so.
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

        if let Some(r) = self.repo_by_path(&path_s)? {
            if self.repo(project_id, r.id).is_ok() {
                return Ok(r);
            }
            let home = self.project(r.home_project_id)?;
            let tx = self.conn.unchecked_transaction()?;
            self.attach_repo(project_id, r.id)?;
            self.write_event(
                project_id, None, actor, "repo_added",
                &format!("{} ({}), which opens on board \"{}\"", r.name, r.path, home.name),
            )?;
            tx.commit()?;
            return Ok(r);
        }

        let claimed: Option<i64> = self.conn.query_row(
            &format!("SELECT project_id FROM project_paths WHERE project_id != ?2 AND {AT_OR_BELOW} LIMIT 1"),
            params![path_s, project_id],
            |r| r.get(0),
        ).optional()?;
        if let Some(other) = claimed {
            let other = self.project(other)?;
            return Err(Error::PathClaimed { path: path_s, board: other.name, key: other.key });
        }

        let fallback = canon.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path_s.clone());
        let name = check_name(name.filter(|n| !n.trim().is_empty()).unwrap_or(&fallback))?;
        self.ensure_repo_name_free(&name, None)?;

        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "INSERT INTO repos (home_project_id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, name, path_s, now()],
        )?;
        let id = self.conn.last_insert_rowid();
        self.attach_repo(project_id, id)?;
        // The half that makes an agent opening this repo land on this board.
        self.add_path_alias(project_id, &canon)?;
        // Not housekeeping: "eee-web joined this board" is history a cold agent can use, and
        // it is the entry that explains why a directory started resolving here.
        self.write_event(project_id, None, actor, "repo_added", &format!("{name} ({path_s})"))?;
        tx.commit()?;
        self.repo(project_id, id)
    }

    fn attach_repo(&self, project_id: i64, repo_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO board_repos (project_id, repo_id, created_at) VALUES (?1, ?2, ?3)",
            params![project_id, repo_id, now()],
        )?;
        Ok(())
    }

    /// Makes this board the repo's home: its folder, and every subdirectory the old home had
    /// learned, resolves here from now on. History already written stays on the board that
    /// wrote it -- this changes where the NEXT session lands, which is the whole question.
    pub fn set_repo_home(&self, project_id: i64, repo_id: i64, actor: Actor) -> Result<Repo> {
        let r = self.repo(project_id, repo_id)?;
        if r.home_project_id == project_id {
            return Ok(r);
        }
        let tx = self.conn.unchecked_transaction()?;
        self.move_home(&r, project_id, actor)?;
        tx.commit()?;
        self.repo(project_id, repo_id)
    }

    /// The shared half of changing a home: re-point the aliases, and say so on both boards --
    /// on each, it is the entry explaining why a folder started or stopped opening there.
    fn move_home(&self, r: &Repo, to: i64, actor: Actor) -> Result<()> {
        let from = self.project(r.home_project_id)?;
        let target = self.project(to)?;
        self.rehome(r, to)?;
        self.write_event(to, None, actor, "repo_home",
            &format!("{} now opens on this board, moved from \"{}\"", r.name, from.name))?;
        self.write_event(from.id, None, actor, "repo_home",
            &format!("{} now opens on board \"{}\"", r.name, target.name))?;
        Ok(())
    }

    /// The data half of a home move, without the events: forgetting a board moves homes too,
    /// and its events must not name the board being forgotten.
    pub(crate) fn rehome(&self, r: &Repo, to: i64) -> Result<()> {
        // Only the old home's aliases move. A subdirectory some third board claimed on its own
        // (split off with a `.ai-kanban` marker, say) was never this repo's to hand over.
        self.conn.execute(
            &format!("UPDATE project_paths SET project_id = ?2 WHERE project_id = ?3 AND {AT_OR_BELOW}"),
            params![r.path, to, r.home_project_id],
        )?;
        // And the root itself, in case the old home never held it. `add_path_alias` never takes
        // a path from another board, so this cannot steal one either.
        self.add_path_alias(to, Path::new(&r.path))?;
        self.conn.execute("UPDATE repos SET home_project_id = ?2 WHERE id = ?1", params![r.id, to])?;
        Ok(())
    }

    /// A repo by id, whichever boards it is on. Only for callers acting on the repo as a whole
    /// -- forgetting it -- where no single board scopes the question.
    pub(crate) fn repo_anywhere(&self, id: i64) -> Result<Repo> {
        let found = self.conn.query_row(
            &format!("SELECT {REPO_COLS} FROM repos WHERE id = ?1"), [id], row_to_repo,
        ).optional()?;
        found.ok_or_else(|| Error::InvalidValue {
            field: "repo",
            value: format!("#{id}"),
            valid: self.all_repos().map(|v| v.into_iter().map(|r| r.name).collect::<Vec<_>>().join(", "))
                .unwrap_or_default(),
        })
    }

    pub fn rename_repo(&self, project_id: i64, id: i64, name: &str, actor: Actor) -> Result<Repo> {
        let before = self.repo(project_id, id)?;
        let name = check_name(name)?;
        if name == before.name {
            return Ok(before);
        }
        self.ensure_repo_name_free(&name, Some(id))?;
        self.conn.execute("UPDATE repos SET name = ?2 WHERE id = ?1", params![id, name])?;
        self.write_event(project_id, None, actor, "repo_renamed", &format!("{} -> {name}", before.name))?;
        self.repo(project_id, id)
    }

    /// Takes a repo off **this board** and off this board's tickets. Returns how many tickets
    /// lost it. Other boards keep it, and keep their tickets' links.
    ///
    /// When this board is the home and others remain, the home passes to the earliest of them,
    /// so the folder keeps opening on a board that has the repo. When this is the last board,
    /// the repo goes, and the folder's path alias is **left in place**: removing a repo from
    /// the menu says it is no longer part of this work, not that its history belongs
    /// somewhere else, and dropping the alias would send the next session in that folder to a
    /// brand-new empty board -- the split this store is built to prevent.
    pub fn remove_repo(&self, project_id: i64, id: i64, actor: Actor) -> Result<usize> {
        let r = self.repo(project_id, id)?;
        let tx = self.conn.unchecked_transaction()?;
        // Explicit rather than left to ON DELETE CASCADE, for the reason `forget.rs` gives:
        // `foreign_keys` is per-connection state, and a link left behind would keep a ticket
        // naming a repo its board no longer has.
        let unlinked = self.conn.execute(
            "DELETE FROM task_repos WHERE repo_id = ?1 AND task_id IN (SELECT id FROM tasks WHERE project_id = ?2)",
            params![id, project_id],
        )?;
        self.conn.execute("DELETE FROM board_repos WHERE project_id = ?1 AND repo_id = ?2", params![project_id, id])?;
        let mut after = String::new();
        if r.home_project_id == project_id {
            let next: Option<i64> = self.conn.query_row(
                "SELECT project_id FROM board_repos WHERE repo_id = ?1 ORDER BY created_at, project_id LIMIT 1",
                [id],
                |row| row.get(0),
            ).optional()?;
            match next {
                Some(next) => {
                    self.move_home(&r, next, actor)?;
                    after = format!("; it now opens on board \"{}\"", self.project(next)?.name);
                }
                None => {
                    self.conn.execute("DELETE FROM repos WHERE id = ?1", [id])?;
                }
            }
        }
        self.write_event(
            project_id, None, actor, "repo_removed",
            &format!("{} ({}), was on {unlinked} ticket{}{after}", r.name, r.path, if unlinked == 1 { "" } else { "s" }),
        )?;
        tx.commit()?;
        Ok(unlinked)
    }

    /// Store-wide: a repo is one thing seen from several boards, so its name has to mean the
    /// same checkout on each of them.
    fn ensure_repo_name_free(&self, name: &str, except: Option<i64>) -> Result<()> {
        let taken: Option<i64> = self.conn.query_row(
            "SELECT id FROM repos WHERE name = ?1", [name], |r| r.get(0),
        ).optional()?;
        match taken {
            Some(id) if Some(id) != except => Err(Error::InvalidValue {
                field: "name",
                value: name.to_string(),
                valid: "a name no other repo already has -- pass one explicitly".into(),
            }),
            _ => Ok(()),
        }
    }

    /// `base`, or `base-2`, `base-3`... -- for an import, which must not fail on a name the
    /// target store already uses for a different checkout.
    pub(crate) fn free_repo_name(&self, base: &str) -> Result<String> {
        let mut candidate = base.to_string();
        let mut n = 1;
        while self.ensure_repo_name_free(&candidate, None).is_err() {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        Ok(candidate)
    }

    /// Repo references as an agent or a person typed them -- names, in any spelling the
    /// normalizer folds, or paths -- turned into ids of repos on this board.
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

    /// Replaces a task's repos. Callers check the board has them first, with `check_repos`.
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

    /// One task's repos, with their paths. Scoped through the TASK's board, which is the one
    /// board a task's links can mean anything on.
    pub fn task_repos(&self, project_id: i64, task_id: i64) -> Result<Vec<Repo>> {
        let sql = format!(
            "SELECT {} FROM task_repos tr
               JOIN repos r ON r.id = tr.repo_id
               JOIN tasks t ON t.id = tr.task_id
              WHERE tr.task_id = ?1 AND t.project_id = ?2
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

    /// Repos and Planio refs for a set of listed tasks on one board, in two queries however
    /// many rows.
    ///
    /// Only tasks that have either appear. Batched for the reason `tags_for` is: the board
    /// lists a few dozen rows, and a per-row lookup is the kind of thing that stops being free
    /// the moment someone raises the cap. Names no column 008 added, so an agent's board keeps
    /// its repos on a store the hook finds between the two migrations.
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
                "SELECT tr.task_id, r.name FROM task_repos tr
                   JOIN repos r ON r.id = tr.repo_id
                   JOIN tasks t ON t.id = tr.task_id
                  WHERE t.project_id = ? AND tr.task_id IN ({holes})
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
