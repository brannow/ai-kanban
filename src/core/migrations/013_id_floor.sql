-- Task and note ids are never handed out twice.
--
-- WHY
--
-- `tasks.id` and `notes.id` are plain INTEGER PRIMARY KEYs, so SQLite gives a new row
-- max(id)+1. Forgetting the newest task deletes it, and the next task gets its id back. Every
-- reference to the forgotten task -- "blocked by #812" in a log line, a commit message --
-- then silently names an unrelated task. Under sync it is worse: (origin, id) is a task's
-- identity on every machine, so a reused id issues the same handle twice (task #1064).
-- Migration 004 fixed exactly this for events.
--
-- HOW
--
-- Not by rebuilding the tables as AUTOINCREMENT, which 004 could do for events: dropping
-- `tasks` cascades into events, notes and task_repos, the rebuild 010 deliberately avoided.
-- Instead every delete records the highest id ever removed, and the insert paths
-- (`Store::fresh_id`) move a new row above that floor. Triggers rather than code in each
-- delete path, so forget_task, forget_board and whatever deletes rows next all keep it
-- without having to remember to.
--
-- The floor starts at the current maximum. What was reused before this migration cannot be
-- recovered; this only stops it from happening again.

CREATE TABLE IF NOT EXISTS id_floor (
    tbl    TEXT    PRIMARY KEY,
    value  INTEGER NOT NULL
);
INSERT OR IGNORE INTO id_floor (tbl, value) VALUES
    ('tasks', COALESCE((SELECT MAX(id) FROM tasks), 0)),
    ('notes', COALESCE((SELECT MAX(id) FROM notes), 0));

CREATE TRIGGER IF NOT EXISTS tasks_id_floor AFTER DELETE ON tasks BEGIN
    UPDATE id_floor SET value = MAX(value, old.id) WHERE tbl = 'tasks';
END;
CREATE TRIGGER IF NOT EXISTS notes_id_floor AFTER DELETE ON notes BEGIN
    UPDATE id_floor SET value = MAX(value, old.id) WHERE tbl = 'notes';
END;
