-- When a row on a connected board was last edited, as a clock value taken at the edit.
--
-- Until now flush stamped every change with the time it ran, so the side that synced last won
-- a conflict, not the side that edited last: a status set offline in the morning and synced in
-- the evening reopened a task a teammate had finished at noon (#1069). The edit time can only be
-- caught where the edit happens, which for every write path is the dirty triggers.
--
-- A side table, and the triggers dropped and recreated rather than altered: like 013-016 this
-- must be safe to apply to a store that already has it (note #178).

CREATE TABLE IF NOT EXISTS sync_edited (
    kind      TEXT    NOT NULL,
    local_id  INTEGER NOT NULL,
    -- Bumped by every edit; only there so an edit of an already dirty row fires the stamp below.
    edits     INTEGER NOT NULL DEFAULT 1,
    wall_ms   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, local_id)
);

-- The stamp, in one place: never behind the wall clock, and always past anything this store
-- has seen (sync_node moves past every received clock), so an edit made after receiving a
-- teammate's change still wins over it even when their clock runs ahead. One millisecond per
-- edit instead of a counter: the clock is a single number per row, and edits do not come
-- faster than that for long. Rows written by applying a teammate's changes fire this too, so a
-- large first pull runs the clock ahead by a millisecond per row: harmless, since a clock ahead
-- only ever wins ties it would otherwise have had to break by tag.
DROP TRIGGER IF EXISTS sync_edited_ai;
CREATE TRIGGER sync_edited_ai AFTER INSERT ON sync_edited BEGIN
    UPDATE sync_node SET wall_ms = MAX(wall_ms + 1, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)), counter = 0 WHERE id = 1;
    UPDATE sync_edited SET wall_ms = (SELECT wall_ms FROM sync_node WHERE id = 1) WHERE kind = NEW.kind AND local_id = NEW.local_id;
END;
DROP TRIGGER IF EXISTS sync_edited_au;
CREATE TRIGGER sync_edited_au AFTER UPDATE OF edits ON sync_edited BEGIN
    UPDATE sync_node SET wall_ms = MAX(wall_ms + 1, CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)), counter = 0 WHERE id = 1;
    UPDATE sync_edited SET wall_ms = (SELECT wall_ms FROM sync_node WHERE id = 1) WHERE kind = NEW.kind AND local_id = NEW.local_id;
END;

-- The dirty triggers of 014, each now also recording the edit.
DROP TRIGGER IF EXISTS sync_tasks_ai;
CREATE TRIGGER sync_tasks_ai AFTER INSERT ON tasks
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', NEW.id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('task', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_tasks_au;
CREATE TRIGGER sync_tasks_au AFTER UPDATE ON tasks
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', NEW.id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('task', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_tasks_left;
CREATE TRIGGER sync_tasks_left AFTER UPDATE OF project_id ON tasks
WHEN OLD.project_id != NEW.project_id
 AND EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = OLD.project_id) BEGIN
    INSERT INTO sync_dirty (kind, local_id, left_from) VALUES ('task', NEW.id, OLD.project_id)
    ON CONFLICT (kind, local_id) DO UPDATE SET left_from = excluded.left_from;
    INSERT INTO sync_edited (kind, local_id) VALUES ('task', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_notes_ai;
CREATE TRIGGER sync_notes_ai AFTER INSERT ON notes
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', NEW.id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('note', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_notes_au;
CREATE TRIGGER sync_notes_au AFTER UPDATE ON notes
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', NEW.id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('note', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_note_paths_ai;
CREATE TRIGGER sync_note_paths_ai AFTER INSERT ON note_paths
WHEN EXISTS (SELECT 1 FROM notes n JOIN sync_rooms r ON r.project_id = n.project_id WHERE n.id = NEW.note_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', NEW.note_id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('note', NEW.note_id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_note_paths_ad;
CREATE TRIGGER sync_note_paths_ad AFTER DELETE ON note_paths
WHEN EXISTS (SELECT 1 FROM notes n JOIN sync_rooms r ON r.project_id = n.project_id WHERE n.id = OLD.note_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', OLD.note_id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('note', OLD.note_id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_events_ai;
CREATE TRIGGER sync_events_ai AFTER INSERT ON events
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('event', NEW.id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('event', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_events_au;
CREATE TRIGGER sync_events_au AFTER UPDATE OF project_id ON events
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('event', NEW.id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('event', NEW.id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_task_repos_ai;
CREATE TRIGGER sync_task_repos_ai AFTER INSERT ON task_repos
WHEN EXISTS (SELECT 1 FROM tasks t JOIN sync_rooms r ON r.project_id = t.project_id WHERE t.id = NEW.task_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', NEW.task_id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('task', NEW.task_id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
DROP TRIGGER IF EXISTS sync_task_repos_ad;
CREATE TRIGGER sync_task_repos_ad AFTER DELETE ON task_repos
WHEN EXISTS (SELECT 1 FROM tasks t JOIN sync_rooms r ON r.project_id = t.project_id WHERE t.id = OLD.task_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', OLD.task_id);
    INSERT INTO sync_edited (kind, local_id) VALUES ('task', OLD.task_id) ON CONFLICT DO UPDATE SET edits = edits + 1;
END;
