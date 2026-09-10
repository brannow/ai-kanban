-- Repos and the Planio reference: where a ticket's work happens, and which external ticket
-- it tracks.
--
-- Why a board OWNS its repos rather than every repo being its own board: a Planio project
-- (a customer, a product) spans several repositories, and a ticket in it touches some of
-- them. With one board per repo, a ticket touching eee-api and eee-web has to live on one of
-- the two, and an agent opening the other never sees it. Registering a repo on a board also
-- registers its root as a path alias, so an agent opening ANY of the board's repos lands on
-- the same board -- `project_paths` already allowed a board to own many paths; this names
-- which of them are repositories.
--
-- `path` is UNIQUE across the store, not per board, for the same reason
-- `project_paths.path` is a primary key: one directory resolves to exactly one board.

CREATE TABLE IF NOT EXISTS repos (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    -- Normalized like a workstream name, because agents type it: `eee-web` in one session
    -- and `EEE_Web` in the next must be one repo, not a near-miss the UNIQUE lets through.
    name        TEXT    NOT NULL,
    path        TEXT    NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL,
    UNIQUE (project_id, name)
);
CREATE INDEX IF NOT EXISTS idx_repos_project ON repos(project_id);

-- Many-to-many: a ticket can touch several repos, and a repo carries many tickets. A table
-- rather than a comma-joined column for the reason 005 gives for workstreams: the repos menu
-- needs a per-repo COUNT, and joined text is not enumerable without splitting every row.
CREATE TABLE IF NOT EXISTS task_repos (
    task_id  INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    repo_id  INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, repo_id)
);
CREATE INDEX IF NOT EXISTS idx_task_repos_repo ON task_repos(repo_id);

-- A number, not free text: Planio issue ids are integers, and a TEXT column would let
-- `48213`, `#48213` and a pasted URL coexist as three spellings of one ticket.
--
-- Deliberately NOT added to TASK_COLS, following 005 and 006: keeping it out of the shared
-- column list is what holds MIN_READABLE_VERSION where it is, so the read-only
-- SessionStart hook keeps rendering against a store this binary has not migrated yet.
ALTER TABLE tasks ADD COLUMN planio INTEGER;

-- Indexed so `recall 48213` finds the ticket -- "have I worked on this before" is exactly
-- the question an agent handed a Planio number should be able to ask. tasks_fts is an
-- external-content table, so a new column means dropping and rebuilding it, as in 006.
DROP TRIGGER IF EXISTS tasks_ai;
DROP TRIGGER IF EXISTS tasks_ad;
DROP TRIGGER IF EXISTS tasks_au;
DROP TABLE IF EXISTS tasks_fts;

CREATE VIRTUAL TABLE tasks_fts USING fts5(
    title, body, tags, planio, content='tasks', content_rowid='id'
);
CREATE TRIGGER tasks_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts(rowid, title, body, tags, planio) VALUES (new.id, new.title, new.body, new.tags, new.planio);
END;
CREATE TRIGGER tasks_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body, tags, planio) VALUES ('delete', old.id, old.title, old.body, old.tags, old.planio);
END;
CREATE TRIGGER tasks_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts(tasks_fts, rowid, title, body, tags, planio) VALUES ('delete', old.id, old.title, old.body, old.tags, old.planio);
    INSERT INTO tasks_fts(rowid, title, body, tags, planio) VALUES (new.id, new.title, new.body, new.tags, new.planio);
END;

-- Without this the index is empty and every existing task becomes unfindable by recall --
-- silently, since an empty FTS index returns no rows rather than an error.
INSERT INTO tasks_fts(tasks_fts) VALUES ('rebuild');
