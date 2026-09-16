//! Schema versioning.
//!
//! # Why this exists
//!
//! Before this, `Store::init` ran `schema.sql` as a batch of `CREATE TABLE IF NOT EXISTS`.
//! That creates a store perfectly well and can never change one. The moment a column is
//! needed that an existing store does not have, there is no way to add it -- and by then
//! every user's store is full of history that cannot be thrown away and recreated, because
//! not throwing history away is the entire point of this project.
//!
//! So the cost of not having this grows with every day the tool is used. It is built now,
//! while the number of real stores is small, and it is built before the HTTP API because
//! that consumer is the first one certain to need a column (see `docs/http-api.md`).
//!
//! # `user_version`, not a migrations table
//!
//! SQLite keeps a 32-bit integer in the database header, free to read and write. A
//! `schema_migrations` table would carry names and timestamps, but it has a bootstrap
//! problem -- the table itself has to be created before it can record that anything was
//! created -- and for a single-user local tool the extra fidelity buys nothing.
//!
//! # The rules
//!
//! * **`schema.sql` is frozen at version 1.** Every change after it is a migration. The
//!   alternative -- keeping `schema.sql` current *and* writing a migration -- means every
//!   change is written twice and the two can disagree, so a fresh install ends up subtly
//!   different from an upgraded one.
//! * **Migrations only run from `Store::open`.** `Store::open_existing` is the read-only
//!   path the hooks use, and hook code must never write. See `min_readable` below.
//! * **Each migration is atomic with its version stamp.** SQLite has transactional DDL, so
//!   a migration that fails leaves the store at the previous version rather than half-way
//!   between two.

use crate::core::error::Result;
use rusqlite::Connection;

const BASELINE: &str = include_str!("schema.sql");

/// The version of the schema in `schema.sql`.
pub const BASELINE_VERSION: i64 = 1;

pub struct Migration {
    pub version: i64,
    pub sql: &'static str,
}

/// Ordered, ascending, contiguous from `BASELINE_VERSION + 1`. `tests/migrate.rs` asserts
/// that, because a gap or a duplicate would silently skip a step on some stores and not
/// others depending on which version they happened to be at.
const MIGRATIONS: &[Migration] = &[
    Migration { version: 2, sql: include_str!("migrations/002_row_version.sql") },
    Migration { version: 3, sql: include_str!("migrations/003_event_note_id.sql") },
    Migration { version: 4, sql: include_str!("migrations/004_events_autoincrement.sql") },
    Migration { version: 5, sql: include_str!("migrations/005_workstreams.sql") },
    Migration { version: 6, sql: include_str!("migrations/006_task_tags.sql") },
    Migration { version: 7, sql: include_str!("migrations/007_repos_planio.sql") },
    Migration { version: 8, sql: include_str!("migrations/008_board_repos.sql") },
    Migration { version: 9, sql: include_str!("migrations/009_board_profiles.sql") },
    Migration { version: 10, sql: include_str!("migrations/010_testing_status.sql") },
];

/// The version this binary brings a store up to.
pub const SCHEMA_VERSION: i64 = if MIGRATIONS.len() == 0 {
    BASELINE_VERSION
} else {
    MIGRATIONS[MIGRATIONS.len() - 1].version
};

/// The oldest version whose structure still satisfies this binary's read queries.
///
/// **Bump this whenever a migration adds a column that a read query then selects** -- which
/// is most of them -- as well as for a dropped or renamed one.
///
/// The intuition to distrust here is that adding a column is backward compatible. It is not,
/// in this direction. Every query in this codebase selects an explicit column list
/// (`TASK_COLS`, `NOTE_COLS`) rather than `SELECT *`, so naming a column that does not exist
/// yet is a hard error. Explicit lists buy safety in the *other* direction: an older binary
/// reading a store that a newer one has migrated simply asks for fewer columns and works.
///
/// This is `2` because migration 002 added `tasks.version`, which `TASK_COLS` now selects.
///
/// Migration 005 deliberately did **not** raise it, and the reasoning is worth keeping
/// because it is the pattern to copy rather than the exception. It adds
/// `tasks.workstream_id`, two tables, and a sticky pointer -- but nothing it adds is ever
/// named in a `SELECT` list that a read path uses. `workstream_id` is used only in `WHERE`
/// and `GROUP BY`, and the sticky pointer went into its own table precisely so that
/// `row_to_project`'s column list would not change. Reads against a store still on 2 or 4
/// therefore work unchanged; they simply find no workstreams and render the whole board,
/// which is exactly the pre-005 behaviour. Cost: one extra table. Bought: the SessionStart
/// hook keeps working through the upgrade instead of going silent for a session.
///
/// # What it buys
///
/// A user upgrades the binary. Their store is still on the old schema, because nothing has
/// written to it yet. The `SessionStart` hook opens it read-only and cannot migrate it -- it
/// must never write -- so its queries would name a column that is not there. Every hook path
/// is `.ok()?`, so that fails silently anyway; the point of this constant is to make the
/// silence *deliberate and declared* rather than an accident of error handling, and to give
/// whoever writes the next migration one place to notice the consequence.
///
/// The degradation is bounded: the MCP server opens the same store for writing moments later
/// in that session and migrates it. At most one `SessionStart` is missed, once, per upgrade.
pub const MIN_READABLE_VERSION: i64 = 2;

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// `PRAGMA` will not take a bound parameter, so this formats. The value is always one of
/// our own `i64` constants, never caller input.
fn set_user_version(conn: &Connection, v: i64) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {v}"))?;
    Ok(())
}

/// Brings a store up to `SCHEMA_VERSION`. Idempotent.
pub(crate) fn run(conn: &Connection) -> Result<()> {
    let mut version = user_version(conn)?;

    if version < BASELINE_VERSION {
        // Version 0 is two different stores that need the same treatment: a brand new file,
        // and a store created before versioning existed. Running the baseline covers both
        // because it is all `IF NOT EXISTS` -- it builds the first and no-ops on the second.
        //
        // Treating an unstamped store as being *at* the baseline is safe only because
        // version 1 is the first version there has ever been, so "unversioned" and "version
        // 1" describe the same schema. That reasoning does not generalise; it is why this
        // branch exists once rather than being a pattern to copy.
        conn.execute_batch(BASELINE)?;
        set_user_version(conn, BASELINE_VERSION)?;
        version = BASELINE_VERSION;
    }

    for m in MIGRATIONS {
        if m.version <= version {
            continue;
        }
        // `unchecked_transaction` because the store hands out `&Connection`, not `&mut`.
        // The DDL and the version stamp commit together or not at all.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(m.sql)?;
        set_user_version(&tx, m.version)?;
        tx.commit()?;
        version = m.version;
    }

    Ok(())
}

/// Whether a read-only consumer can trust its queries against this store.
///
/// `false` means "say nothing", not "report an error" -- the caller is a hook.
pub(crate) fn is_readable(conn: &Connection) -> Result<bool> {
    Ok(user_version(conn)? >= MIN_READABLE_VERSION)
}

/// Exposed for tests, which need to assert the list is well-formed without being able to
/// see a private const.
pub fn migration_versions() -> Vec<i64> {
    MIGRATIONS.iter().map(|m| m.version).collect()
}

/// The frozen version-1 schema, exposed so tests can build a genuine pre-versioning store.
///
/// Simulating one by rewinding `user_version` on a current store does not work and is worth
/// stating: the rewound store still has every column the later migrations added, so
/// re-running them fails on a duplicate column. That is not a bug in the runner -- it is a
/// state no real store can be in. A truthful test has to start from this.
pub fn baseline_sql() -> &'static str {
    BASELINE
}
