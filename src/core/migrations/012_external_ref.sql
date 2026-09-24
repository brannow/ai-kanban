-- `planio` becomes `external_ref`: the ticket in whatever outside tracker a task tracks.
--
-- 007 named one vendor in the schema. The concept is not Planio's -- it is "this task mirrors
-- an issue somewhere else" -- and a Planio-only INTEGER column cannot hold a Jira key
-- (`PROJ-123`), a GitHub issue or anything else that is not a bare number. So the column is
-- TEXT and says nothing about which tracker. Which tracker it is, and where its issues live,
-- is the person's setting (`AI_KANBAN_REF_URL`), not the store's.
--
-- TEXT gives up the one thing the INTEGER bought: `48213`, `#48213` and a pasted URL could
-- coexist as spellings of one ticket. `core::task::check_external_ref` takes that job over --
-- it strips a leading `#` and refuses whitespace and URLs -- so both adapters store one
-- spelling.
--
-- WHY A NEW MIGRATION AND NOT AN EDIT TO 007
--
-- 007 has already run on the stores it shipped to. `migrate::run` skips every migration at or
-- below a store's `user_version`, so an edited 007 would never reach them and this binary
-- would ask those stores for a column they do not have. Migrations are append-only.

ALTER TABLE tasks ADD COLUMN external_ref TEXT;
UPDATE tasks SET external_ref = CAST(planio AS TEXT) WHERE planio IS NOT NULL;

-- The FTS table's column list is fixed at creation, and the triggers name `planio` -- which
-- also blocks the DROP COLUMN below, since SQLite refuses to drop a column a trigger names.
-- So both go first and are rebuilt over the new column, as 006 and 007 did.
DROP TRIGGER IF EXISTS tasks_ai;
DROP TRIGGER IF EXISTS tasks_ad;
DROP TRIGGER IF EXISTS tasks_au;
DROP TABLE IF EXISTS tasks_fts;

-- DROP COLUMN rewrites the table in place. It is not the DROP TABLE that 010 had to avoid:
-- no rows are deleted, so no foreign-key action fires and task_repos and blocked_by survive.
ALTER TABLE tasks DROP COLUMN planio;

CREATE VIRTUAL TABLE tasks_fts USING fts5(
    title, body, tags, external_ref, content='tasks', content_rowid='id'
);
CREATE TRIGGER tasks_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, title, body, tags, external_ref) VALUES (new.id, new.title, new.body, new.tags, new.external_ref);
END;
CREATE TRIGGER tasks_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body, tags, external_ref) VALUES ('delete', old.id, old.title, old.body, old.tags, old.external_ref);
END;
CREATE TRIGGER tasks_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body, tags, external_ref) VALUES ('delete', old.id, old.title, old.body, old.tags, old.external_ref);
    INSERT INTO tasks_fts(rowid, title, body, tags, external_ref) VALUES (new.id, new.title, new.body, new.tags, new.external_ref);
END;

-- Without this the new index is empty and every task becomes unfindable by recall, silently.
INSERT INTO tasks_fts(tasks_fts) VALUES ('rebuild');
