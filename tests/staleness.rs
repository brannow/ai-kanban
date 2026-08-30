//! Notes whose subject is gone.
//!
//! The hard half of this feature is not detecting a deleted file, it is *declining* to
//! claim one was deleted. Most of these tests are about the silence.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::Store;

/// A project with a real directory behind it, so paths can actually resolve.
fn project(dir: &std::path::Path) -> (Store, i64, std::path::PathBuf) {
    let s = Store::open_in_memory().unwrap();
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let pid = s.resolve_project(&root).unwrap().project.id;
    (s, pid, root)
}

fn note_about(s: &Store, pid: i64, title: &str, paths: &[&str]) -> i64 {
    s.create_note(pid, NoteDraft {
        paths: paths.iter().map(|p| p.to_string()).collect(),
        ..NoteDraft::new(title)
    }, Actor::Agent).unwrap().id
}

#[test]
fn a_note_about_a_deleted_file_is_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, root) = project(tmp.path());
    std::fs::write(root.join("src/kept.rs"), "fn main() {}").unwrap();

    let alive = note_about(&s, pid, "about a file that exists", &["src/kept.rs"]);
    let dead = note_about(&s, pid, "about a file that was removed", &["src/gone.rs"]);

    let missing = s.missing_subjects(pid, &[alive, dead]).unwrap();
    assert!(missing.for_note(alive).is_empty(), "a present file must not be flagged");
    assert_eq!(missing.for_note(dead), ["src/gone.rs"]);
}

#[test]
fn nothing_is_claimed_when_no_path_resolves_at_all() {
    // The calibration, and the reason this feature is usable. On a machine where the
    // project was never checked out, every path is missing for a reason that has nothing to
    // do with the notes. Flagging them all is the noise failure that makes a reader stop
    // trusting the flag -- which destroys the true positives too.
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, _) = project(tmp.path());
    let a = note_about(&s, pid, "one", &["src/a.rs"]);
    let b = note_about(&s, pid, "two", &["src/b.rs"]);

    let missing = s.missing_subjects(pid, &[a, b]).unwrap();
    assert!(missing.is_empty(), "with no resolvable path anywhere, silence is the honest answer");
}

#[test]
fn one_resolvable_path_is_enough_to_license_the_others() {
    // The flip side: the calibration must not be so cautious that it never fires. A single
    // hit proves the path convention works here, which is what makes the misses meaningful.
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, root) = project(tmp.path());
    std::fs::write(root.join("src/present.rs"), "").unwrap();

    let anchor = note_about(&s, pid, "anchor", &["src/present.rs"]);
    let gone = note_about(&s, pid, "gone", &["src/removed.rs"]);

    let missing = s.missing_subjects(pid, &[anchor, gone]).unwrap();
    assert_eq!(missing.for_note(gone), ["src/removed.rs"]);
}

#[test]
fn a_file_under_any_of_the_projects_roots_counts_as_present() {
    // project_paths exists because one project legitimately has several checkouts. A file
    // present in the worktree is present, even if the original clone is gone.
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, root) = project(tmp.path());
    std::fs::write(root.join("src/present.rs"), "").unwrap();

    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::write(worktree.join("src/only-here.rs"), "").unwrap();
    s.add_path_alias(pid, &worktree).unwrap();

    let n = note_about(&s, pid, "lives in the worktree", &["src/only-here.rs"]);
    let anchor = note_about(&s, pid, "anchor", &["src/present.rs"]);

    let missing = s.missing_subjects(pid, &[n, anchor]).unwrap();
    assert!(missing.for_note(n).is_empty(), "a file in a second checkout is not deleted");
}

#[test]
fn an_absolute_path_is_checked_as_written() {
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, root) = project(tmp.path());
    std::fs::write(root.join("src/present.rs"), "").unwrap();
    let anchor = note_about(&s, pid, "anchor", &["src/present.rs"]);

    let abs = tmp.path().join("nowhere/absent.rs");
    let n = note_about(&s, pid, "absolute", &[abs.to_str().unwrap()]);

    let missing = s.missing_subjects(pid, &[anchor, n]).unwrap();
    assert_eq!(missing.for_note(n).len(), 1);
}

#[test]
fn a_note_with_no_paths_is_never_flagged() {
    // Most notes carry no path at all. They must cost nothing and claim nothing.
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, root) = project(tmp.path());
    std::fs::write(root.join("src/present.rs"), "").unwrap();
    let anchor = note_about(&s, pid, "anchor", &["src/present.rs"]);
    let bare = note_about(&s, pid, "a general lesson", &[]);

    let missing = s.missing_subjects(pid, &[anchor, bare]).unwrap();
    assert!(missing.for_note(bare).is_empty());
}

#[test]
fn the_flag_reaches_the_rendered_recall() {
    // End to end: core computing it is worth nothing if the agent never sees it.
    use ai_kanban::core::recall::RecallQuery;
    let tmp = tempfile::tempdir().unwrap();
    let (s, pid, root) = project(tmp.path());
    std::fs::write(root.join("src/present.rs"), "").unwrap();
    note_about(&s, pid, "anchor about the redirect guard", &["src/present.rs"]);
    note_about(&s, pid, "the redirect guard lived here", &["src/deleted.rs"]);

    let r = s.recall(&RecallQuery { text: "redirect guard", project_id: Some(pid), limit: 10 }).unwrap();
    let notes: Vec<i64> = r.hits.iter().filter(|h| h.kind == HitKind::Note).map(|h| h.id).collect();
    let missing = s.missing_subjects(pid, &notes).unwrap();
    let text = ai_kanban::render::recall(&r, false, &missing);

    assert!(text.contains("no longer in the repo: src/deleted.rs"), "got:\n{text}");
    assert!(!text.contains("src/present.rs)"), "the surviving file must not be listed as gone");
}
