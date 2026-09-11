-- Which session profiles a board may start from the board page.
--
-- Boards are projects, and the Claude Code setup a project belongs in is the person's call:
-- a customer board opens in the work login, a private one must not. So it is set per board.
--
-- A deny list rather than an allow list, so a board nobody has configured allows every
-- profile -- the behaviour before this existed -- and no marker is needed to tell "never set"
-- from "set to nothing".
--
-- Its own table rather than a column on `projects`, for the reason `current_workstream` is:
-- `row_to_project` is on the read-only hook path, and a new column there would force a
-- MIN_READABLE_VERSION bump.
CREATE TABLE IF NOT EXISTS board_denied_profiles (
    project_id  INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    -- A launch profile's name (`http::launch::Profile`). Not constrained here: the store does
    -- not know which profiles exist, the launcher does, and it validates before writing.
    profile     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (project_id, profile)
);
