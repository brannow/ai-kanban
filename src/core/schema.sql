-- ai-kanban schema (v1)
--
-- Conventions, stated once so nothing below has to re-explain itself:
--
--  * Timestamps are unix epoch SECONDS (INTEGER, UTC). Chosen over ISO-8601 TEXT because
--    every read does age arithmetic ("2h ago", "note is 6 months old") and none of it
--    should involve parsing or timezones.
--  * Task IDs are GLOBAL, not per-project. A per-project counter would render nicer
--    (#1, #2 in a fresh project) but it makes an ID meaningless without its project,
--    which breaks cross-project recall where hits from several projects interleave.
--    It would also need SELECT MAX()+1 under a transaction; multiple sessions write
--    concurrently under WAL, and that races. rowid autoincrement does not.
--  * Every ORDER BY on a timestamp carries an `id` tie-break. Timestamps are whole
--    seconds, so anything created in the same second ties -- and a hundred rows written
--    in one second is not a corner case, it is a seeded board or a busy minute. SQLite
--    then returns tied rows in whatever order it likes, which makes listings unstable
--    between calls and tests flaky in a way that looks like a logic bug.
--  * Enumerations use CHECK constraints rather than free TEXT so a wrong value fails
--    loudly at write time. The adapter turns the failure into a message listing the
--    valid values, per the design law that errors self-correct.

PRAGMA journal_mode = WAL;      -- concurrent sessions across projects, no daemon
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS projects (
    id          INTEGER PRIMARY KEY,
    key         TEXT    NOT NULL UNIQUE,   -- stable identity: git remote URL, else root path
    name        TEXT    NOT NULL,          -- display name, usually the directory basename
    created_at  INTEGER NOT NULL
);

-- A project owns MANY paths. This is what stops a worktree, a second clone, or a moved
-- folder from silently forking the board into two half-memories.
CREATE TABLE IF NOT EXISTS project_paths (
    path        TEXT    PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_project_paths_project ON project_paths(project_id);

CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title       TEXT    NOT NULL,
    body        TEXT    NOT NULL DEFAULT '',
    status      TEXT    NOT NULL DEFAULT 'backlog'
                CHECK (status IN ('backlog','doing','blocked','done','archived')),
    type        TEXT    NOT NULL DEFAULT 'task'
                CHECK (type IN ('task','bug','idea','chore')),
    -- Instrumentation for priority #2 (agent self-organization). Defaults to 'agent';
    -- the agent passes 'user' when relaying a request. Biased toward 'agent' by design --
    -- see docs/data-model.md on why the count is a floor, not a measurement.
    origin      TEXT    NOT NULL DEFAULT 'agent' CHECK (origin IN ('user','agent')),
    -- Words, not numbers. A numeric priority forces the agent to know whether low means
    -- urgent, which is exactly the internal knowledge the design law forbids.
    priority    TEXT    NOT NULL DEFAULT 'normal'
                CHECK (priority IN ('low','normal','high','urgent')),
    -- Annotation only. `status` is authoritative: status='blocked' with blocked_by=NULL is
    -- legal and means "blocked on something outside the board".
    blocked_by  INTEGER REFERENCES tasks(id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_project_status ON tasks(project_id, status);
CREATE INDEX IF NOT EXISTS idx_tasks_updated ON tasks(project_id, updated_at DESC);

-- Append-only. NULL task_id means project-level history (a decision, a session summary)
-- that needs no task to hang off. Its reader is the board snapshot's `recent` section --
-- an append-only log nobody reads is exactly the extra work agents correctly skip.
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id     INTEGER REFERENCES tasks(id) ON DELETE CASCADE,
    ts          INTEGER NOT NULL,
    actor       TEXT    NOT NULL DEFAULT 'agent' CHECK (actor IN ('user','agent','system')),
    kind        TEXT    NOT NULL,   -- task_created | status_changed | note_updated | log | ...
    body        TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_events_project_ts ON events(project_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id, ts DESC);

-- Durable knowledge about the code. No lifecycle, outlives the task that produced it.
CREATE TABLE IF NOT EXISTS notes (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id     INTEGER REFERENCES tasks(id) ON DELETE SET NULL,
    title       TEXT    NOT NULL,
    body        TEXT    NOT NULL DEFAULT '',
    tags        TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL   -- recall shows hit age from this, so stale claims can be discounted
);
CREATE INDEX IF NOT EXISTS idx_notes_project ON notes(project_id, updated_at DESC);

-- Associates a note with the files it is about. NOTHING CONSUMES THIS IN V1.
-- It ships anyway because proactive contextual recall ("you're editing auth/middleware.rs,
-- here's what you learned about it") is the form of memory that actually gets used, and
-- retrofitting the association means re-tagging every note ever written. Cheap now,
-- expensive later.
CREATE TABLE IF NOT EXISTS note_paths (
    note_id     INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    path        TEXT    NOT NULL,
    PRIMARY KEY (note_id, path)
);
CREATE INDEX IF NOT EXISTS idx_note_paths_path ON note_paths(path);

-- ---------------------------------------------------------------------------
-- FTS5. External-content tables: the index stores only the terms, rows stay in
-- the base tables, kept in sync by triggers. Avoids storing every body twice.
-- ---------------------------------------------------------------------------

CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
    title, body, tags, content='notes', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, body, tags) VALUES ('delete', old.id, old.title, old.body, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, body, tags) VALUES ('delete', old.id, old.title, old.body, old.tags);
    INSERT INTO notes_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(
    title, body, content='tasks', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS tasks_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;
CREATE TRIGGER IF NOT EXISTS tasks_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
END;
CREATE TRIGGER IF NOT EXISTS tasks_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
    INSERT INTO tasks_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    body, content='events', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events BEGIN
    INSERT INTO events_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events BEGIN
    INSERT INTO events_fts(events_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
