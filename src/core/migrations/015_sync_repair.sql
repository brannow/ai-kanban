-- Brings every store that ran an earlier draft of migration 014 to what 014 creates today.
--
-- WHY
--
-- 014 was edited after it had already run somewhere: a development binary migrated a real
-- store while 014 lacked `sync_own_tags` and `sync_unmapped_repos`. A store at version 14 never
-- runs 014 again (note #178), so on that store every forget, repo removal and tag change would
-- fail on a missing table. 014 is frozen from here; this repeats what was added to it, with
-- IF NOT EXISTS, so a store built by the final 014 and one built by the draft end up identical.

CREATE TABLE IF NOT EXISTS sync_own_tags (
    tag  TEXT PRIMARY KEY
);
-- A tag set by the draft was never recorded as this store's own.
INSERT OR IGNORE INTO sync_own_tags (tag) SELECT tag FROM sync_node WHERE id = 1 AND tag IS NOT NULL;

CREATE TABLE IF NOT EXISTS sync_unmapped_repos (
    task_id  INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    key      TEXT    NOT NULL,
    PRIMARY KEY (task_id, key)
);
