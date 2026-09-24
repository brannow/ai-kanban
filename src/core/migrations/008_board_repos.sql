-- Repos shared between boards, each with exactly one home.
--
-- 007 gave every repo a single board. The model people actually work in is different: a
-- board is a project (a Planio project, a customer), and a checkout can serve several of
-- them -- a shared library, a monorepo two projects deploy from. So repos become global, tied
-- to any number of boards through `board_repos`.
--
-- What cannot be shared is where a folder RESOLVES. An agent opening a directory has to land
-- on one board, the same one every time, or the same folder reads a different memory on
-- different days -- the split this store exists to prevent. So each repo keeps one HOME
-- board, the one its path alias points at, and that is what `project_id` becomes. The home
-- is always one of the repo's boards; `core::repo` keeps that true.

ALTER TABLE repos RENAME COLUMN project_id TO home_project_id;
DROP INDEX IF EXISTS idx_repos_project;
CREATE INDEX IF NOT EXISTS idx_repos_home ON repos(home_project_id);

-- Names become unique across the store: a repo is now one thing seen from several boards,
-- and `eee-api` must mean the same checkout on every board that lists it. 007 kept them
-- unique per board only, so a store may hold duplicates by now -- the later one is suffixed
-- with its id rather than failing the migration and stranding the store at 7.
UPDATE repos SET name = name || '-' || id
 WHERE EXISTS (SELECT 1 FROM repos older WHERE older.name = repos.name AND older.id < repos.id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_repos_name ON repos(name);

CREATE TABLE IF NOT EXISTS board_repos (
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_id     INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    -- When the home board lets a repo go, the home passes to the earliest remaining board.
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, repo_id)
);
CREATE INDEX IF NOT EXISTS idx_board_repos_repo ON board_repos(repo_id);

-- Every existing repo is on the board that owned it, which becomes its home.
INSERT OR IGNORE INTO board_repos (project_id, repo_id, created_at)
    SELECT home_project_id, id, created_at FROM repos;
