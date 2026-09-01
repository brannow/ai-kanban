-- Workstreams: a grouping DIMENSION inside a board, not a second board.
--
-- Why this exists: board() had exactly two selection knobs, status and limit, and ordered
-- by recency. Measured on 300 tasks across 12 workstreams, the 30 listed rows came from
-- 4 workstreams chosen purely by recency, and the one being worked on contributed 1 row.
-- A cold agent was told, in detail, about work that was not its own. See note #16.
--
-- Why not separate boards, which is the intuitive fix: `blocked_by` is same-project by
-- design (task.rs rejects a cross-project blocker), so splitting features onto their own
-- boards would break cross-feature blocking -- and project resolution is path-derived, so
-- a finished feature's board becomes unreachable. Grouping inside one board keeps both.

CREATE TABLE IF NOT EXISTS workstreams (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    -- Closing hides a workstream from the directory without touching its tasks. It matters
    -- for the DIRECTORY, not for the tasks: a site maintained for three years would
    -- otherwise list forty dead workstreams on every session start. Task-level collapsing
    -- is already handled by `status` -- done work occupies zero rows on the board.
    closed_at   INTEGER,
    -- One row per name per board. This is what makes the set ENUMERABLE, which a
    -- comma-joined tags column is not: the directory needs `name, COUNT(*)` on every
    -- session start, and that is not answerable from joined text without splitting every
    -- row in the project.
    UNIQUE (project_id, name)
);
CREATE INDEX IF NOT EXISTS idx_workstreams_project ON workstreams(project_id, closed_at);

-- Deliberately NOT added to TASK_COLS. Filtering and grouping need this column in WHERE
-- and GROUP BY, never in a SELECT list -- and keeping it out of the shared column list is
-- what avoids raising MIN_READABLE_VERSION (see note #13 and migrate.rs). The consequence
-- is real: the SessionStart hook keeps working against a store this binary has not
-- migrated yet, instead of going silent for one session per upgrade.
ALTER TABLE tasks ADD COLUMN workstream_id INTEGER REFERENCES workstreams(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_tasks_workstream ON tasks(project_id, workstream_id, status);

-- The sticky pointer lives in its own table rather than as a column on `projects` for the
-- same reason: `row_to_project` selects an explicit column list and is on the read-only
-- hook path, so a new column there would force a MIN_READABLE_VERSION bump.
CREATE TABLE IF NOT EXISTS current_workstream (
    project_id    INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    workstream_id INTEGER NOT NULL REFERENCES workstreams(id) ON DELETE CASCADE,
    set_at        INTEGER NOT NULL
);
