//! Deciding when a note has stopped being about anything.
//!
//! # Why this is deletion and nothing else
//!
//! "Confidently wrong memory is worse than none" is the reason to want this, and it is also
//! the reason most versions of it are a bad idea. Three were considered and rejected before
//! this one, and the reasoning is worth keeping because each looks obviously right until you
//! push on it:
//!
//! * **Last-commit time from git.** Accurate, and it needs a subprocess per path.
//!   `project.rs` reads git's config by hand precisely to avoid that ("no subprocess per
//!   resolution, and no dependency on git being installed"); there is no
//!   `process::Command` anywhere in this codebase, and this feature is not the one to
//!   introduce it.
//! * **File mtime.** A fresh clone rewrites every mtime, so every note in the project
//!   reads as stale at once. A signal that fires on all 40 notes is not a signal.
//! * **A content hash recorded when the note is written.** The most accurate answer to
//!   "did the file change", and the wrong question. A note saying "the middleware rewrites
//!   Location headers" survives a reformat, a comment, a rename elsewhere in the file --
//!   all of which move the hash. A flag that fires constantly trains its reader to ignore
//!   it, which destroys the true positives along with the false ones. It also costs a
//!   migration and file reads on the hook path.
//!
//! Deletion is different in kind. A note about a file that is not there is a note about
//! code that is not there, and that is true regardless of what the note claims. The check
//! is a `stat` with no baseline, no migration and no subprocess.
//!
//! # The calibration, which is the load-bearing part
//!
//! Stored note paths are usually repo-relative (`src/core/store.rs`) while the store is
//! global, so a path only resolves against one of the project's roots -- and on a machine
//! where the project was never checked out, *nothing* resolves. Reporting "missing" there
//! would flag every note in the board for a reason that has nothing to do with the notes.
//! This repo's own budget fixture is exactly that case: 40 notes, none of whose paths exist
//! on disk.
//!
//! So a claim is only made when this project's paths are demonstrably resolvable here: at
//! least one path in the set being checked has to be found. If none are, the convention
//! does not apply on this machine and the honest output is silence. Absence of evidence is
//! not evidence of deletion.

use crate::core::error::Result;
use crate::core::store::Store;
use std::path::{Path, PathBuf};

/// Which of a note's files are gone, for the notes being shown.
///
/// Checked as a batch rather than per note because the calibration above is a property of
/// the whole set: one resolvable path anywhere in it is what licenses every "missing" in
/// it. Per-note checking cannot know that.
#[derive(Debug, Clone, Default)]
pub struct MissingSubjects {
    /// note id -> the paths that could not be found.
    missing: std::collections::HashMap<i64, Vec<String>>,
}

impl MissingSubjects {
    /// Empty when nothing is known to be gone -- including the uncalibrated case, where
    /// nothing is knowable.
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty()
    }

    pub fn for_note(&self, note_id: i64) -> &[String] {
        self.missing.get(&note_id).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

impl Store {
    /// The roots a repo-relative note path could be relative to.
    ///
    /// A project can own several (a worktree, a second clone, the original), which is the
    /// whole point of `project_paths`. A file present under any of them is present.
    fn project_roots(&self, project_id: i64) -> Result<Vec<PathBuf>> {
        Ok(self.project_paths(project_id)?.into_iter().map(PathBuf::from).collect())
    }

    /// Notes whose files are gone, among `note_ids`.
    ///
    /// Returns an empty result rather than an error whenever the answer is not knowable:
    /// no roots, no resolvable paths, or a filesystem that will not answer. Every caller of
    /// this is decorating output that is useful without it.
    pub fn missing_subjects(&self, project_id: i64, note_ids: &[i64]) -> Result<MissingSubjects> {
        let roots = self.project_roots(project_id)?;
        if roots.is_empty() {
            return Ok(MissingSubjects::default());
        }

        let mut found_any = false;
        let mut candidates: std::collections::HashMap<i64, Vec<String>> = Default::default();

        for &id in note_ids {
            for path in self.note_paths(id)? {
                if resolves(&roots, &path) {
                    found_any = true;
                } else {
                    candidates.entry(id).or_default().push(path);
                }
            }
        }

        // The calibration. Without a single hit anywhere in this set, "not found" means the
        // checkout is not here, not that the code was deleted.
        if !found_any {
            return Ok(MissingSubjects::default());
        }
        Ok(MissingSubjects { missing: candidates })
    }
}

/// A stored path is either already absolute, or relative to one of the project's roots.
fn resolves(roots: &[PathBuf], stored: &str) -> bool {
    let p = Path::new(stored);
    if p.is_absolute() {
        return p.exists();
    }
    roots.iter().any(|r| r.join(p).exists())
}
