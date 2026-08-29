-- Make event ids monotonic, so the change cursor can never go sideways.
--
-- WHY
--
-- `docs/http-api.md` uses `MAX(events.id)` as the change cursor and `events.id` as the SSE
-- `Last-Event-ID`. Both assume ids only ever climb. That was true for free while events were
-- append-only -- and it stopped being true the moment `forget` started deleting them.
--
-- A plain INTEGER PRIMARY KEY is the rowid, and SQLite reuses the largest rowid after it is
-- deleted. So `forget_note` deletes the note's events (frequently including the newest) and
-- the tombstone it writes next is handed the same id back. MAX(id) is unchanged, a polling
-- client concludes nothing happened, and a live page keeps showing a note that is gone.
-- Reused ids would also make SSE resume replay the wrong events.
--
-- AUTOINCREMENT is exactly the guarantee needed: ids are monotonic and never reused. It
-- cannot be added by ALTER TABLE, so the table is rebuilt. Cheap now; this table is the one
-- that grows forever.
--
-- Row ids are copied verbatim, so nothing that references an event by id is invalidated,
-- and sqlite_sequence picks up the high-water mark from the explicit inserts.

CREATE TABLE events_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id     INTEGER REFERENCES tasks(id) ON DELETE CASCADE,
    note_id     INTEGER REFERENCES notes(id) ON DELETE CASCADE,
    ts          INTEGER NOT NULL,
    actor       TEXT    NOT NULL DEFAULT 'agent' CHECK (actor IN ('user','agent','system')),
    kind        TEXT    NOT NULL,
    body        TEXT    NOT NULL DEFAULT ''
);

INSERT INTO events_new (id, project_id, task_id, note_id, ts, actor, kind, body)
     SELECT id, project_id, task_id, note_id, ts, actor, kind, body FROM events;

-- Dropping the table takes its indexes and triggers with it; both are recreated below.
DROP TABLE events;
ALTER TABLE events_new RENAME TO events;

CREATE INDEX IF NOT EXISTS idx_events_project_ts ON events(project_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_note ON events(note_id);

CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events BEGIN
    INSERT INTO events_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
    INSERT INTO events_fts(events_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;

-- The external-content index still points at rows that were copied out from under it.
-- Rebuilding is the only way to be sure it matches, and it is what keeps a forgotten note's
-- body from surviving in the search index.
INSERT INTO events_fts(events_fts) VALUES('rebuild');
