-- The person's to-do list: global, and the one thing in this store the agent cannot touch.
--
-- WHY IT IS NOT TASKS
--
-- A task is the agent's working memory: it has a status the agent moves, an origin the
-- project measures itself by, a workstream, repos, a blocker. A to-do is none of that. It is
-- "renew the domain" -- a person's own list, kept beside the work rather than inside it.
-- Modelled as a task it would need a project, appear on a board an agent reads cold, and cost
-- tokens on every session start to tell the agent about errands it can do nothing about.
--
-- WHY GLOBAL AND NOT PER BOARD
--
-- No `project_id`, deliberately. The list is the same list from every board, which is what
-- was asked for and also what the data wants: a person does not re-open their errands per
-- repository. It also means the list survives `forget_board`, which cascades everything
-- carrying a project_id.
--
-- WHY NO EVENTS
--
-- Every mutation in this store writes an event (`tests/change_feed.rs`), and this table is the
-- deliberate exception. The events table is the agent's history -- it feeds `recent` on the
-- board an agent reads cold -- and `events.project_id` is NOT NULL, so a global to-do has no
-- honest row to write there. Putting errands in front of the agent is the opposite of what
-- this list is for.
--
-- The cost is that the live page cannot see to-do changes through `MAX(events.id)`, which is
-- what `todo_rev` below repairs.
CREATE TABLE IF NOT EXISTS todos (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    text        TEXT    NOT NULL,
    -- NULL = not checked. A timestamp rather than a flag because checked items are swept a
    -- day later, and "when was it checked" is the only thing that can answer that.
    done_at     INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- The change counter for the live page, standing in for the events table this list does not
-- write to.
--
-- A counter rather than `MAX(updated_at)` for the reason every ORDER BY here carries an id
-- tie-break: timestamps are whole seconds, so checking two items in one second would leave
-- the second one invisible to an open page. A counter rather than `MAX(id)` because deleting
-- a row -- checking one off and sweeping it -- would move that number backwards.
CREATE TABLE IF NOT EXISTS todo_rev (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    rev  INTEGER NOT NULL
);
INSERT OR IGNORE INTO todo_rev (id, rev) VALUES (1, 0);
