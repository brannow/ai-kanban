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
//! 1. `.ai-kanban` marker file, found walking up. Explicit beats inferred -- this is the
//!    escape hatch for monorepo packages and non-git directories.
//! 2. A known path in `project_paths`. Hit -> done, no filesystem work at all.
//! 3. Git root, keyed on the normalized remote URL if there is one, else the root path.
//! 4. No git, no marker -> the starting directory itself becomes the project.
//!
//! Note what is absent: `roots/list`. SEP-2577 (Final) deprecates it, and names environment
//! variables as a replacement, so the caller's path comes from `CLAUDE_PROJECT_DIR` first
//! and cwd second. Roots may be consulted opportunistically by the adapter, never here.

use crate::core::error::Result;
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
        let start = canonical(start);

        // 1. Known path. Checked before any filesystem walk: it is the hot path, and a
        //    learned alias must win over re-deriving an identity that might differ.
        if let Some(p) = self.project_by_path(&start)? {
            return Ok(Resolved { project: p, how: Resolution::KnownPath, created: false });
        }

        // 2..4 -- derive an identity from the filesystem.
        let (key, name, root, how) = derive_identity(&start);

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

    pub fn all_projects(&self) -> Result<Vec<Project>> {
        let mut st = self.conn.prepare(
            "SELECT id, key, name, created_at FROM projects ORDER BY name",
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
        Ok((p, true))
    }

    /// Idempotent: a path already claimed by another project is left alone rather than
    /// stolen. Silently reassigning would move history out from under the other board.
    pub fn add_path_alias(&self, project_id: i64, path: &Path) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO project_paths (path, project_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![path.to_string_lossy(), project_id, now()],
        )?;
        Ok(())
    }

    pub fn project_paths(&self, project_id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT path FROM project_paths WHERE project_id = ?1 ORDER BY created_at",
        )?;
        let rows = st.query_map([project_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project { id: r.get(0)?, key: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
}

/// Walks up from `start` deriving a stable identity. Pure filesystem logic, no DB --
/// separated so it can be tested against fixture directories.
fn derive_identity(start: &Path) -> (String, String, PathBuf, Resolution) {
    for dir in start.ancestors() {
        // Marker first at each level: explicit beats inferred, and a monorepo package
        // marker sits below the git root that would otherwise swallow it.
        let marker = dir.join(MARKER_FILE);
        if let Some(key) = read_marker(&marker) {
            let name = key.rsplit('/').next().unwrap_or(&key).to_string();
            return (key, name, dir.to_path_buf(), Resolution::Marker);
        }
        if dir.join(".git").exists() {
            let name = basename(dir);
            return match git_remote(dir) {
                // Keyed on the remote: a second clone, a moved folder and a worktree all
                // land on the same board.
                Some(url) => (format!("git:{}", normalize_remote(&url)), name, dir.to_path_buf(), Resolution::GitRemote),
                None => (format!("path:{}", dir.to_string_lossy()), name, dir.to_path_buf(), Resolution::GitRoot),
            };
        }
    }
    (format!("path:{}", start.to_string_lossy()), basename(start), start.to_path_buf(), Resolution::Directory)
}

fn read_marker(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    // First non-empty, non-comment line is the key. Anything else in the file is free-form.
    let key = raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    Some(key.to_string())
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
