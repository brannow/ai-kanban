//! Which session profiles a board may start. See migration 009.
//!
//! The store keeps names and nothing else. Which profiles exist, and what starting one means,
//! belongs to the launcher (`http::launch::Profile`), which validates a name before it gets here.

use crate::core::error::Result;
use crate::core::model::Actor;
use crate::core::store::{now, Store};
use rusqlite::params;

impl Store {
    /// The profiles this board refuses, by name. None refused means every profile is allowed.
    pub fn denied_profiles(&self, project_id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT profile FROM board_denied_profiles WHERE project_id = ?1 ORDER BY profile",
        )?;
        let rows = st.query_map([project_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Idempotent, and writes an event only when something changed: a board page re-sending the
    /// setting it already shows is not history.
    pub fn set_profile_allowed(&self, project_id: i64, profile: &str, allowed: bool, actor: Actor) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = if allowed {
            tx.execute(
                "DELETE FROM board_denied_profiles WHERE project_id = ?1 AND profile = ?2",
                params![project_id, profile],
            )?
        } else {
            tx.execute(
                "INSERT OR IGNORE INTO board_denied_profiles (project_id, profile, created_at) VALUES (?1, ?2, ?3)",
                params![project_id, profile, now()],
            )?
        };
        if changed > 0 {
            let kind = if allowed { "profile_allowed" } else { "profile_denied" };
            self.write_event(project_id, None, actor, kind, profile)?;
        }
        tx.commit()?;
        Ok(())
    }
}
