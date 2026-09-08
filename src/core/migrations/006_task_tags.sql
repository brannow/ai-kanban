-- Task tags: freeform, multi-valued, human-facing labels.
--
-- Why this rather than custom statuses, which is what people ask for: every status carries
-- BEHAVIOUR, not just a label. is_open() decides what "8 open" means on the board an agent
-- reads cold, board_rank decides ordering, `blocked` is tied to blocked_by, `archived` is
-- terminal. A user-defined status has no answer to "is this open, should I pick work from
-- it" except in the user's head -- which inverts the design law: the agent would need
-- knowledge of the USER'S configuration, worse than needing knowledge of ours. And because
-- the store is global and recall crosses projects, one board's "in review" against
-- another's "reviewing" quietly makes cross-project search meaningless.
--
-- Tags carry no semantics an agent must honour, so it can ignore them safely, while a
-- person still gets "in review", "waiting-on-vendor", "frontend". That is the whole point:
-- the escape valve exists so the five statuses can stay fixed.
--
-- NOT the storage for workstreams, which is a separate feature in migration 005. A
-- workstream must be ENUMERABLE (the directory needs name + count per scope) and
-- single-valued per task (a default view needs one answer to "which scope am I in"). A
-- comma-joined TEXT column serves neither.

-- Deliberately NOT added to TASK_COLS, following 005. Keeping it out of the shared column
-- list is what avoids raising MIN_READABLE_VERSION: the SessionStart hook opens the store
-- read-only and cannot migrate it, so a bump costs one silent session-start per upgrade in
-- every repo the user opens. Tags are read by task_show and the web UI, both on paths that
-- have already migrated, so a separate lookup costs nothing the shared list would save.
ALTER TABLE tasks ADD COLUMN tags TEXT NOT NULL DEFAULT '';

-- tasks_fts is an EXTERNAL-CONTENT table, so its column list is fixed at creation and the
-- only way to add one is to drop and rebuild. Tags are indexed rather than filterable on
-- purpose: recall's contract is "plain words -- no search syntax needed", and a `tag:x`
-- operator would be a second query language beside FTS5's, and exactly the internal
-- knowledge the design law forbids. Indexed, a tag is findable by typing it.
DROP TRIGGER IF EXISTS tasks_ai;
DROP TRIGGER IF EXISTS tasks_ad;
DROP TRIGGER IF EXISTS tasks_au;
DROP TABLE IF EXISTS tasks_fts;

CREATE VIRTUAL TABLE tasks_fts USING fts5(
    title, body, tags, content='tasks', content_rowid='id'
);
CREATE TRIGGER tasks_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER tasks_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body, tags) VALUES ('delete', old.id, old.title, old.body, old.tags);
END;
CREATE TRIGGER tasks_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body, tags) VALUES ('delete', old.id, old.title, old.body, old.tags);
    INSERT INTO tasks_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;

-- Without this the index is empty and every existing task becomes unfindable by recall --
-- silently, since an empty FTS index returns no rows rather than an error.
INSERT INTO tasks_fts(tasks_fts) VALUES ('rebuild');
