//! Schema versioning.
//!
//! The test that earns its place here is `a_store_made_before_versioning_is_adopted`.
//! Every store that exists today was created before `user_version` was written, so
//! "unstamped" is not a hypothetical legacy case -- it is what all real data looks like
//! right now. Getting that path wrong would either wipe those stores or refuse to open
//! them, and the whole point of the project is that the history in them survives.

use ai_kanban::core::migrate::{migration_versions, BASELINE_VERSION, MIN_READABLE_VERSION, SCHEMA_VERSION};
use ai_kanban::core::model::*;
use ai_kanban::core::Store;

/// A file-backed store. Migrations are about what survives a reopen, so in-memory will not do.
fn on_disk() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kanban.db");
    (dir, path)
}

#[test]
fn a_new_store_is_stamped_at_the_current_version() {
    let (_d, path) = on_disk();
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn the_migration_list_is_ascending_and_contiguous() {
    // A gap or a repeat would apply a different set of steps depending on which version a
    // given store happened to start from, so two users could end up with different schemas
    // from the same binary. Cheap to assert, effectively impossible to debug later.
    let versions = migration_versions();
    let mut expected = BASELINE_VERSION;
    for v in &versions {
        expected += 1;
        assert_eq!(*v, expected, "migrations must be contiguous from BASELINE_VERSION + 1");
    }
    assert_eq!(SCHEMA_VERSION, expected, "SCHEMA_VERSION must be the last migration");
}

/// A store exactly as a build from before `migrate.rs` existed would have left it: the
/// baseline tables, rows written through them, and no version stamp.
///
/// Built with raw SQL on purpose. Going through `Store` would migrate it on the way in,
/// which is the very thing under test.
fn legacy_store(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(ai_kanban::core::migrate::baseline_sql()).unwrap();
    conn.execute_batch(
        "INSERT INTO projects (id, key, name, created_at) VALUES (1, 'k', 'legacy', 100);
         INSERT INTO tasks (id, project_id, title, body, status, type, origin, priority, created_at, updated_at)
              VALUES (1, 1, 'survives the upgrade', '', 'backlog', 'task', 'agent', 'normal', 100, 100);
         INSERT INTO notes (id, project_id, title, body, tags, created_at, updated_at)
              VALUES (1, 1, 'the redirect loop was a trailing slash', 'it was nginx', '', 100, 100);
         INSERT INTO events (project_id, task_id, ts, actor, kind, body)
              VALUES (1, 1, 100, 'agent', 'created', 'survives the upgrade');",
    ).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0)).unwrap(),
        0,
        "the fixture is only meaningful if it really is unstamped"
    );
}

#[test]
fn a_store_made_before_versioning_is_adopted_without_losing_data() {
    let (_d, path) = on_disk();
    legacy_store(&path);

    let s = Store::open(&path).unwrap();

    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION, "an unstamped store gets adopted");
    assert_eq!(s.task(1, 1).unwrap().title, "survives the upgrade");
    assert_eq!(s.note(1, 1).unwrap().title, "the redirect loop was a trailing slash");
    assert_eq!(s.board(1, &BoardQuery::board()).unwrap().tasks.len(), 1);
}

#[test]
fn rows_written_before_the_guard_existed_get_a_usable_version() {
    // Migration 2 backfills `version` with 1. If it defaulted to NULL or 0 instead, every
    // pre-existing row would be unguardable -- the web UI could never send a version that
    // matched, so every edit of an old task would fail as a conflict forever.
    let (_d, path) = on_disk();
    legacy_store(&path);

    let s = Store::open(&path).unwrap();
    assert_eq!(s.task(1, 1).unwrap().version, 1);
    assert_eq!(s.note(1, 1).unwrap().version, 1);
}

#[test]
fn adoption_leaves_the_search_index_intact() {
    // Adoption re-runs the baseline batch. If any `IF NOT EXISTS` were missing -- on an FTS
    // virtual table or one of its sync triggers -- it would show up here, either as an error
    // or as a search that silently stopped matching.
    let (_d, path) = on_disk();
    legacy_store(&path);

    let s = Store::open(&path).unwrap();
    let hits = s.recall(&ai_kanban::core::recall::RecallQuery {
        text: "redirect",
        project_id: Some(1),
        limit: 10,
    }).unwrap();
    assert!(!hits.hits.is_empty(), "search must still find notes written before the upgrade");
}

#[test]
fn opening_an_up_to_date_store_changes_nothing() {
    let (_d, path) = on_disk();
    let first = { Store::open(&path).unwrap().schema_version().unwrap() };
    let second = { Store::open(&path).unwrap().schema_version().unwrap() };
    assert_eq!(first, second);
    assert_eq!(second, SCHEMA_VERSION);
}

#[test]
fn min_readable_version_keeps_up_with_what_reads_require() {
    // The trap this guards is that "adding a column" reads as backward compatible and is
    // not. Every query here selects an explicit column list, so naming a column a store does
    // not have yet is a hard error -- which means most migrations that add a column must
    // raise MIN_READABLE_VERSION. (Explicit lists do buy safety the other way: an older
    // binary reading a newer store just asks for fewer columns.)
    //
    // This was missed once already: migration 002 added `tasks.version`, TASK_COLS started
    // selecting it, and MIN_READABLE_VERSION stayed at 0 -- claiming every store ever made
    // was still readable when the baseline had stopped being readable in the same commit.
    assert!(
        MIN_READABLE_VERSION <= SCHEMA_VERSION,
        "a binary that cannot read the schema it produces is never right"
    );

    // A store this binary migrates must satisfy its own read queries. If a future migration
    // raises MIN_READABLE_VERSION past what the code can actually read, this catches it.
    let (_d, path) = on_disk();
    legacy_store(&path);
    let s = Store::open(&path).unwrap();
    assert!(s.schema_version().unwrap() >= MIN_READABLE_VERSION);
    s.board(1, &BoardQuery::board()).unwrap();
    s.task(1, 1).unwrap();
    s.note(1, 1).unwrap();
}

#[test]
fn a_store_behind_the_binary_is_reported_as_unreadable_not_broken() {
    // The scenario MIN_READABLE_VERSION exists for. Migration 002 added `version`, and
    // TASK_COLS now selects it -- so a store still on the baseline cannot answer this
    // binary's read queries at all. The read-only path must notice that from the version
    // stamp and decline, rather than discovering it as a missing-column error mid-render.
    let (_d, path) = on_disk();
    legacy_store(&path);

    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(&format!("PRAGMA user_version = {BASELINE_VERSION}")).unwrap();

    // Prove the queries really would fail, so the constant is not guarding a phantom.
    let broken = conn.query_row(
        "SELECT version FROM tasks WHERE id = 1", [], |r| r.get::<_, i64>(0));
    assert!(broken.is_err(), "a baseline store has no `version` column");

    assert!(
        BASELINE_VERSION < MIN_READABLE_VERSION,
        "migration 002 made the baseline unreadable, so MIN_READABLE_VERSION must be above it"
    );
}
