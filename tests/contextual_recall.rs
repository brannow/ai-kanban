//! `PostToolUse` contextual recall, including the half that reads through Bash.
//!
//! The gap this covers (#8): `PostToolUse` gives `Read`/`Edit`/`Write` a `file_path` and
//! gives `Bash` only a `command` string. With a `Read|Edit|Write` matcher, every file an
//! agent opened with `cat`, `sed`, `head` or `grep` surfaced no notes at all — and that is
//! how agents mostly read, because this project's own `CLAUDE.md` and Claude Code's auto
//! mode both tell them to. Half the recall mechanism was silently not firing, which is worse
//! than not having it: nothing indicates the memory is being skipped.

use ai_kanban::core::model::*;
use ai_kanban::core::note::NoteDraft;
use ai_kanban::core::Store;
use ai_kanban::hook;

/// A project directory with one real file in it, and a note about that file.
fn fixture(rel: &str) -> (Store, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(rel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;
    s.create_note(
        pid,
        NoteDraft { paths: vec![rel.to_string()], body: "it was a trailing slash".into(), ..NoteDraft::new("the redirect loop") },
        Actor::Agent,
    ).unwrap();
    (s, dir, rel.to_string())
}

#[test]
fn a_file_read_with_sed_surfaces_its_notes() {
    let (s, dir, rel) = fixture("src/core/note.rs");

    let files = hook::paths_in_command(&format!("sed -n '1,50p' {rel}"), dir.path());
    let ctx = hook::file_context(&s, dir.path(), &files, None)
        .expect("a note about this file must surface even though it was not read with Read");

    assert!(ctx.contains("the redirect loop"));
    assert!(ctx.contains("note_update"), "and it must still say what to do if it is wrong");
}

#[test]
fn the_same_note_arrives_whether_the_file_was_read_or_catted() {
    let (s, dir, rel) = fixture("src/core/note.rs");
    let abs = dir.path().join(&rel).to_string_lossy().into_owned();

    let via_read = hook::file_context(&s, dir.path(), &[abs], None).unwrap();
    let via_bash = hook::file_context(
        &s, dir.path(), &hook::paths_in_command(&format!("cat {rel}"), dir.path()), None,
    ).unwrap();

    assert_eq!(via_read, via_bash, "which tool the agent happened to use must not change memory");
}

#[test]
fn a_command_naming_no_real_file_says_nothing() {
    // The existence check is the only filter that matters, so this is the one that has to
    // hold: a shell command full of path-shaped tokens that are not files must stay silent.
    let (s, dir, _) = fixture("src/core/note.rs");

    for command in [
        "cargo test --test budget",
        "git log --oneline -1",
        "rustc --version",
        "echo 'src/core/nonexistent.rs'",
        "grep -rn 'note.rs' .",
    ] {
        let files = hook::paths_in_command(command, dir.path());
        assert!(
            hook::file_context(&s, dir.path(), &files, None).is_none(),
            "{command:?} should surface nothing, got files {files:?}"
        );
    }
}

#[test]
fn flags_and_shell_punctuation_are_not_mistaken_for_paths() {
    let (_s, dir, rel) = fixture("src/core/note.rs");

    let found = hook::paths_in_command(
        &format!("cat {rel} | grep -n 'fn' > /dev/null 2>&1"),
        dir.path(),
    );

    assert_eq!(found.len(), 1, "only the real source file, got {found:?}");
    assert!(found[0].ends_with("note.rs"));
}

#[test]
fn several_files_in_one_command_are_all_considered() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;

    for (rel, title) in [("a.rs", "about a"), ("b.rs", "about b")] {
        std::fs::write(dir.path().join(rel), "x\n").unwrap();
        s.create_note(
            pid,
            NoteDraft { paths: vec![rel.to_string()], ..NoteDraft::new(title) },
            Actor::Agent,
        ).unwrap();
    }

    let files = hook::paths_in_command("cat a.rs b.rs", dir.path());
    let ctx = hook::file_context(&s, dir.path(), &files, None).unwrap();

    assert!(ctx.contains("about a") && ctx.contains("about b"), "got: {ctx}");
}

#[test]
fn one_command_never_costs_more_than_one_read() {
    // The cap is on the whole message, not per file. A command touching four documented
    // files must not deliver four times the interruption -- this hook fires on every tool
    // call, and volume is what gets it uninstalled.
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory().unwrap();
    let pid = s.resolve_project(dir.path()).unwrap().project.id;

    let mut names = Vec::new();
    for i in 0..4 {
        let rel = format!("f{i}.rs");
        std::fs::write(dir.path().join(&rel), "x\n").unwrap();
        for n in 0..3 {
            s.create_note(
                pid,
                NoteDraft { paths: vec![rel.clone()], ..NoteDraft::new(format!("note {i}-{n}")) },
                Actor::Agent,
            ).unwrap();
        }
        names.push(rel);
    }

    let files = hook::paths_in_command(&format!("cat {}", names.join(" ")), dir.path());
    let ctx = hook::file_context(&s, dir.path(), &files, None).unwrap();

    assert_eq!(ctx.matches("note #").count(), 3, "at most three notes total, got:\n{ctx}");
    assert!(ctx.contains("more (recall to see them)"), "and the rest are accounted for");
}

#[test]
fn an_absolute_path_in_a_command_resolves() {
    let (s, dir, rel) = fixture("src/core/note.rs");
    let abs = dir.path().join(&rel).to_string_lossy().into_owned();

    let files = hook::paths_in_command(&format!("head -20 {abs}"), dir.path());
    assert!(hook::file_context(&s, dir.path(), &files, None).is_some());
}

#[test]
fn a_directory_is_not_a_file() {
    // `grep -r pattern src/` names a directory. Notes are filed against files, and treating
    // a directory as one would match every note whose path sits under it.
    let (_s, dir, _) = fixture("src/core/note.rs");
    let found = hook::paths_in_command("grep -rn pattern src/", dir.path());
    assert!(found.is_empty(), "got {found:?}");
}

#[test]
fn a_huge_command_is_bounded() {
    // A heredoc or a generated command can be arbitrarily long, and this runs on every
    // shell call. Without a bound it would stat every token.
    let (_s, dir, rel) = fixture("src/core/note.rs");
    let padding = (0..500).map(|i| format!("tok{i}.x")).collect::<Vec<_>>().join(" ");
    let found = hook::paths_in_command(&format!("{padding} {rel}"), dir.path());
    assert!(found.len() <= 4, "got {found:?}");
}
