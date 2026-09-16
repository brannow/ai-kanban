//! Project resolution: turning "where the caller is" into a stable project identity.
//!
//! # Why this is the most careful file in the codebase
//!
//! If the same project resolves to two different identities, memory silently splits into
//! two half-boards and neither is right. Nothing errors, nothing warns -- the agent just
//! stops finding what it wrote last week. That is the failure this whole file exists to
//! prevent, and it is why the project is resolved from a *path* rather than from a name
//! the model supplies: an LLM writing an identifier is non-deterministic (`ai-kanban`
//! today, `ai_kanban` tomorrow), while a path resolves the same way every time.
//!
//! The mitigation is alias learning (every path that resolves to a project is recorded,
//! so a worktree or a second clone joins the existing board instead of starting a new one).
//! The detection is that every response states the project it resolved.
//!
//! # Order
//!
//! Resolution is a single walk upward from the caller's directory, in two passes:
//!
//! 1. `.ai-kanban` marker file, all the way up first. Explicit beats inferred -- the escape
//!    hatch for monorepo packages and non-git directories.
//!
//! Then, at **each** level:
//!
//! 2. A known path in `project_paths`. A board already claimed this directory.
//! 3. A `.git` directory. Keyed on the normalized remote URL if there is one, else the root
//!    path.
//!
//! If the walk finds nothing, the starting directory becomes its own project -- unless it is
//! `$HOME`, a directory above it, a filesystem root or the temp dir, which is refused. Those
//! sit above every project, and a board anchored there becomes a known path that rule 2
//! matches for *every* non-git directory beneath it: distinct projects silently merge into
//! one board named after the user. A marker or a `.git` there still resolves normally --
//! the refusal is only for the fallback, where nothing says the directory was meant.
//!
//! That ordering carries real weight:
//!
//! * Checking learned aliases at every level (not just the starting directory) is what
//!   stops a subdirectory of a **non-git** project from becoming its own board. Without it
//!   `~/notes` and `~/notes/drafts` are two separate memories, because with no `.git` to
//!   mark a root the walk has nothing else to anchor on.
//! * Giving markers a pass of their own, before any alias, is what keeps a monorepo package
//!   from being swallowed by the repo-wide board above it -- including when the package
//!   already learned an alias before the marker was added (see `walk`).
//!
//! Note what is absent: `roots/list`. SEP-2577 (Final) deprecates it, and names environment
//! variables as a replacement, so the caller's path comes from `CLAUDE_PROJECT_DIR` first
//! and cwd second. Roots may be consulted opportunistically by the adapter, never here.

use crate::core::error::{Error, Result};
use crate::core::model::Project;
use crate::core::store::{now, Store};
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

pub const MARKER_FILE: &str = ".ai-kanban";

/// How a project identity was arrived at. Carried so responses can explain themselves and
/// so tests can assert on the *path taken*, not just the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Path was already known. The common case after the first call.
    KnownPath,
    /// A `.ai-kanban` marker named the project explicitly.
    Marker,
    /// Keyed on the git remote URL -- survives moves, clones and worktrees.
    GitRemote,
    /// Git repo with no remote. Keyed on the repo root path.
    GitRoot,
    /// Not a repo at all. Keyed on the directory itself.
    Directory,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub project: Project,
    pub how: Resolution,
    /// True when this call created the project. Lets the adapter say "new board" once.
    pub created: bool,
}

impl Store {
    /// Resolves `start` to a project, creating it if unknown, and recording `start` as an
    /// alias either way. Recording on every call is what makes the second clone join the
    /// first one's board.
    pub fn resolve_project(&self, start: &Path) -> Result<Resolved> {
        self.resolve_project_from(start, dirs::home_dir().as_deref())
    }

    /// `resolve_project` with the home directory passed in rather than read from the
    /// environment. Tests need a fake `$HOME`, and setting `HOME` for real is process-wide
    /// under a multithreaded test runner.
    pub fn resolve_project_from(&self, start: &Path, home: Option<&Path>) -> Result<Resolved> {
        let start = canonical(start);

        let (key, name, root, how) = match self.walk(&start)? {
            // A board already owns this directory or one above it.
            Some(Found::Known(project)) => {
                // Learn the starting path so the next call short-circuits on the first
                // level instead of walking again.
                self.add_path_alias(project.id, &start)?;
                return Ok(Resolved { project, how: Resolution::KnownPath, created: false });
            }
            Some(Found::Identity(id)) => id,
            // Only here, not in the walk. A board that already claims one of these
            // directories must still resolve, or the user cannot even read it to repair it.
            None if is_above_every_project(&start, home.map(canonical).as_deref()) => {
                return Err(Error::SharedDirectory { path: start.to_string_lossy().into_owned() });
            }
            None => (format!("path:{}", start.to_string_lossy()), basename(&start), start.clone(), Resolution::Directory),
        };

        let (project, created) = self.upsert_project(&key, &name)?;
        // Both the starting path and the derived root become aliases. Registering the root
        // means a sibling subdirectory resolves by lookup next time instead of re-deriving.
        self.add_path_alias(project.id, &start)?;
        if root != start {
            self.add_path_alias(project.id, &root)?;
        }
        Ok(Resolved { project, how, created })
    }

    pub fn project_by_path(&self, path: &Path) -> Result<Option<Project>> {
        let p = self.conn.query_row(
            "SELECT p.id, p.key, p.name, p.created_at
               FROM project_paths pp JOIN projects p ON p.id = pp.project_id
              WHERE pp.path = ?1",
            [path.to_string_lossy()],
            row_to_project,
        ).optional()?;
        Ok(p)
    }

    pub fn project_by_key(&self, key: &str) -> Result<Option<Project>> {
        Ok(self.conn.query_row(
            "SELECT id, key, name, created_at FROM projects WHERE key = ?1",
            [key],
            row_to_project,
        ).optional()?)
    }

    /// A board named by key or by name, for consumers that have no directory to resolve
    /// from -- the CLI, and the named half of the MCP adapter's `Scope`.
    ///
    /// Key is tried first and exactly: it is unique by construction, so an exact key match
    /// is never ambiguous. Name is matched case-insensitively and may hit several boards
    /// (two clones both called `api` is the normal way a split looks), and that is returned
    /// as an ambiguity rather than resolved. Picking one silently would act on a board the
    /// caller did not mean, and nothing downstream would ever surface it.
    pub fn project_by_name_or_key(&self, query: &str) -> Result<Project> {
        let q = query.trim();
        if let Some(p) = self.project_by_key(q)? {
            return Ok(p);
        }
        let all = self.all_projects()?;
        let matches: Vec<&Project> = all.iter().filter(|p| p.name.eq_ignore_ascii_case(q)).collect();
        match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => Err(crate::core::Error::ProjectNotFound {
                query: q.to_string(),
                existing: all.iter().map(|p| p.name.clone()).collect(),
            }),
            _ => Err(crate::core::Error::AmbiguousProject {
                query: q.to_string(),
                candidates: matches.iter().map(|p| format!("{} ({})", p.name, p.key)).collect(),
            }),
        }
    }

    pub fn all_projects(&self) -> Result<Vec<Project>> {
        let mut st = self.conn.prepare(
            "SELECT id, key, name, created_at FROM projects ORDER BY name, id",
        )?;
        let rows = st.query_map([], row_to_project)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn upsert_project(&self, key: &str, name: &str) -> Result<(Project, bool)> {
        if let Some(p) = self.project_by_key(key)? {
            return Ok((p, false));
        }
        self.conn.execute(
            "INSERT INTO projects (key, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![key, name, now()],
        )?;
        let p = self.project_by_key(key)?.expect("just inserted");
        // A board coming into existence is history, and it is also a change the HTTP live
        // stream has to see -- it polls MAX(events.id), so a new project that wrote no event
        // would simply never appear until something else happened on it.
        self.write_event(p.id, None, crate::core::model::Actor::System, "project_created", name)?;
        Ok((p, true))
    }

    /// Idempotent: a path already claimed by another project is left alone rather than
    /// stolen. Silently reassigning would move history out from under the other board.
    pub fn add_path_alias(&self, project_id: i64, path: &Path) -> Result<()> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO project_paths (path, project_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![path.to_string_lossy(), project_id, now()],
        )?;
        // Only when a path is genuinely new. `INSERT OR IGNORE` reports 0 rows for a path
        // already known -- which is most calls, since this runs on every resolve -- so the
        // event fires once per path rather than once per session.
        //
        // Recorded because alias learning is the mechanism that keeps a board from splitting
        // in two, and until now it happened with no trace at all: if a directory ended up
        // attached to the wrong board there was nothing saying when, or from where. It is
        // filtered out of the agent's `recent` (see `HOUSEKEEPING_KINDS`) -- this is for the
        // history and the live stream, not for the board.
        if inserted > 0 {
            self.write_event(
                project_id, None, crate::core::model::Actor::System,
                "path_learned", &path.to_string_lossy(),
            )?;
        }
        Ok(())
    }

    pub fn project_paths(&self, project_id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT path FROM project_paths WHERE project_id = ?1 ORDER BY created_at, path",
        )?;
        let rows = st.query_map([project_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project { id: r.get(0)?, key: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
}

/// A derived project identity: key, display name, the directory it was derived from, and
/// how it was arrived at.
type Identity = (String, String, PathBuf, Resolution);

/// The outcome of one upward walk.
enum Found {
    /// A board already claims this directory or one above it.
    Known(Project),
    /// No board yet, but the filesystem says what one here would be called.
    Identity(Identity),
}

fn read_marker(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    // First non-empty, non-comment line is the key. Anything else in the file is free-form.
    let key = raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    Some(key.to_string())
}

/// A closed set derived from the environment, deliberately. A heuristic like "a directory
/// that already contains boards" would also catch `~/Projects`, but it would make whether
/// a directory can get a board depend on store state that changes over time.
///
/// The temp dirs go beyond the `$HOME` case that was actually reported, on the same
/// mechanism: a board anchored at `/tmp` would absorb every scratch directory under it.
///
/// Degrades open: a home that cannot be canonicalized is compared as given, so at worst
/// the guard misses and resolution behaves as it did before the guard existed.
fn is_above_every_project(dir: &Path, home: Option<&Path>) -> bool {
    // `Path::starts_with` compares whole components: `/Users/alice` is below `/Users`,
    // not below `/Users/al`.
    dir.parent().is_none()
        || home.is_some_and(|h| h.starts_with(dir))
        // Both, because they differ on macOS: `temp_dir()` is the per-user `$TMPDIR` under
        // /var/folders, while `/tmp` is where a person actually types `cd`.
        || [std::env::temp_dir(), PathBuf::from("/tmp")].iter().any(|t| canonical(t) == dir)
}

fn basename(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn canonical(p: &Path) -> PathBuf {
    // Resolves symlinks and `..`, so /tmp and /private/tmp cannot become two projects.
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Reads `remote.origin.url` from git's config by hand rather than shelling out to `git`:
/// no subprocess per resolution, and no dependency on git being installed.
///
/// Handles the worktree case, where `.git` is a *file* pointing at
/// `<main>/.git/worktrees/<name>`. Getting this wrong is precisely how a worktree forks
/// into its own board -- the failure `project_paths` exists to prevent.
fn git_remote(repo_root: &Path) -> Option<String> {
    let dot_git = repo_root.join(".git");
    let config_path = if dot_git.is_dir() {
        dot_git.join("config")
    } else {
        let contents = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = contents.strip_prefix("gitdir:")?.trim();
        let gitdir = PathBuf::from(gitdir);
        let gitdir = if gitdir.is_absolute() { gitdir } else { repo_root.join(gitdir) };
        // `commondir` points back at the main repo's .git, which owns the shared config.
        match std::fs::read_to_string(gitdir.join("commondir")) {
            Ok(c) => {
                let c = c.trim();
                let common = PathBuf::from(c);
                let common = if common.is_absolute() { common } else { gitdir.join(common) };
                common.join("config")
            }
            Err(_) => gitdir.join("config"),
        }
    };

    let text = std::fs::read_to_string(config_path).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            // Both `[remote "origin"]` and the subsection-less form seen in some configs.
            in_origin = t.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if in_origin {
            if let Some(rest) = t.strip_prefix("url") {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// Collapses the many spellings of one remote into a single key.
///
/// `git@github.com:me/repo.git` and `https://github.com/me/repo` are the same project.
/// Without this, cloning over SSH on one machine and HTTPS on another forks the board --
/// the split-memory failure, arriving by the least obvious route.
pub fn normalize_remote(url: &str) -> String {
    let u = url.trim();
    let u = u.strip_suffix('/').unwrap_or(u);
    // scp-style: git@host:path
    let rest = if let Some(idx) = u.find("://") {
        &u[idx + 3..]
    } else if let Some(idx) = u.find(':') {
        // Only treat as scp-style when it is not a bare drive letter or port form.
        let (head, tail) = u.split_at(idx);
        if head.contains('/') { u } else { return normalize_remote_parts(head, &tail[1..]); }
    } else {
        u
    };
    let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
    match rest.split_once('/') {
        Some((host, path)) => normalize_remote_parts(host, path),
        None => rest.to_lowercase(),
    }
}

fn normalize_remote_parts(host: &str, path: &str) -> String {
    let host = host.split_once('@').map(|(_, h)| h).unwrap_or(host);
    // Drop any port -- github.com and github.com:22 are one host.
    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
    let path = path.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    format!("{}/{}", host.to_lowercase(), path.trim_end_matches('/').to_lowercase())
}

impl Store {
    /// Resolves a path to a project **without creating one**.
    ///
    /// This exists for the session-start hook, which runs in every directory the user ever
    /// opens Claude Code in. Using `resolve_project` there would quietly mint a board for
    /// every scratch folder, tarball and dotfiles checkout they visit -- filling
    /// `board(project: "all")` with noise and turning the store's contents into a record of
    /// where the user has been rather than what they work on.
    ///
    /// Aliases are not learned here either: learning is a side effect of *using* a board,
    /// and merely starting a session in a directory is not use.
    pub fn find_project(&self, start: &Path) -> Result<Option<Project>> {
        let start = canonical(start);
        match self.walk(&start)? {
            Some(Found::Known(p)) => Ok(Some(p)),
            // A derived identity is only a *candidate*: it names a board that may not exist
            // yet, and this function must never bring one into being.
            Some(Found::Identity((key, _, _, _))) => self.project_by_key(&key),
            None => Ok(None),
        }
    }

    /// The single upward walk both resolvers share, so the read-only path and the creating
    /// path can never disagree about which board a directory belongs to.
    fn walk(&self, start: &Path) -> Result<Option<Found>> {
        // Markers are resolved in a full pass of their own, before any alias is considered.
        //
        // Interleaving them per level looks equivalent and is not: an alias learned in a
        // directory *below* a marker would short-circuit the walk before ever reaching the
        // marker's level. Concretely, an agent works in a monorepo package, the user later
        // adds `.ai-kanban` to split that package onto its own board -- and nothing happens,
        // because the deeper alias answers first. A documented escape hatch that silently
        // stops working is worse than not having one.
        //
        // The cost is a handful of `stat` calls on a path that would otherwise hit the
        // database immediately. That is worth paying for "explicit always beats inferred".
        for dir in start.ancestors() {
            if let Some(key) = read_marker(&dir.join(MARKER_FILE)) {
                let name = key.rsplit('/').next().unwrap_or(&key).to_string();
                return Ok(Some(Found::Identity((key, name, dir.to_path_buf(), Resolution::Marker))));
            }
        }
        for dir in start.ancestors() {
            if let Some(p) = self.project_by_path(dir)? {
                return Ok(Some(Found::Known(p)));
            }
            if dir.join(".git").exists() {
                let name = basename(dir);
                return Ok(Some(Found::Identity(match git_remote(dir) {
                    Some(url) => (format!("git:{}", normalize_remote(&url)), name, dir.to_path_buf(), Resolution::GitRemote),
                    None => (format!("path:{}", dir.to_string_lossy()), name, dir.to_path_buf(), Resolution::GitRoot),
                })));
            }
        }
        Ok(None)
    }
}
