-- Sync state for boards connected to a room. `docs/sync.md` is the design.
--
-- Everything lives in side tables, so `tasks`, `notes` and `events` keep their shape and
-- `TASK_COLS` / `MIN_READABLE_VERSION` are untouched (notes #13, #17, #71). A board that is not
-- connected never gets a row in any of them.

-- This store's identity towards the relay: the origin tag its key carries, and its hybrid
-- logical clock, which only ever moves forward.
CREATE TABLE IF NOT EXISTS sync_node (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    tag      TEXT,
    wall_ms  INTEGER NOT NULL DEFAULT 0,
    counter  INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO sync_node (id) VALUES (1);

-- Which board is connected to which room. One room per board, one board per room.
CREATE TABLE IF NOT EXISTS sync_rooms (
    project_id    INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    room          TEXT    NOT NULL UNIQUE,
    cursor        INTEGER NOT NULL DEFAULT 0,
    connected_at  INTEGER NOT NULL
);

-- A local row's identity on every machine: who created it, and the id it got there. Written
-- explicitly for this store's own rows too, never implied by "no row": after a restore the
-- store gets a new tag, and implied ownership would relabel all its old work under it.
CREATE TABLE IF NOT EXISTS sync_ids (
    kind       TEXT    NOT NULL,
    local_id   INTEGER NOT NULL,
    origin     TEXT    NOT NULL,
    origin_id  INTEGER NOT NULL,
    PRIMARY KEY (kind, local_id),
    UNIQUE (kind, origin, origin_id)
);

-- The last value of each synced field as published to or received from a room, with the clock
-- that wrote it. Flushing compares against it, so a field applied from a teammate is not sent
-- back out, and newest-wins compares against its clock. Keyed by room: an entity entering a
-- different room has published nothing there yet.
CREATE TABLE IF NOT EXISTS sync_fields (
    room       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    origin     TEXT    NOT NULL,
    origin_id  INTEGER NOT NULL,
    field      TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    wall_ms    INTEGER NOT NULL,
    counter    INTEGER NOT NULL,
    tag        TEXT    NOT NULL,
    PRIMARY KEY (room, kind, origin, origin_id, field)
);

-- Rows that changed on a connected board since the last flush. Filled by the triggers below,
-- so every write path is covered without having to remember to be: forget, merge, import, the
-- HTTP API and whatever comes next. `left_from` marks a task that moved off a connected board.
CREATE TABLE IF NOT EXISTS sync_dirty (
    kind       TEXT    NOT NULL,
    local_id   INTEGER NOT NULL,
    left_from  INTEGER,
    PRIMARY KEY (kind, local_id)
);

-- Changes waiting to be pushed. AUTOINCREMENT because the seq is half of each change's id on
-- the relay, and a drained outbox would otherwise hand the same seq out again.
CREATE TABLE IF NOT EXISTS sync_outbox (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL,
    room        TEXT    NOT NULL,
    body        TEXT    NOT NULL
);

-- Received changes this binary could not apply (a status it does not know yet, an unknown
-- field). Kept, counted, and re-applied after an upgrade.
CREATE TABLE IF NOT EXISTS sync_parked (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL,
    body        TEXT    NOT NULL,
    reason      TEXT    NOT NULL,
    at          INTEGER NOT NULL
);

-- Entities forgotten in a room. Kept forever, so a change that arrives after the tombstone
-- cannot bring one back.
CREATE TABLE IF NOT EXISTS sync_forgotten (
    room       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    origin     TEXT    NOT NULL,
    origin_id  INTEGER NOT NULL,
    PRIMARY KEY (room, kind, origin, origin_id)
);

-- ---------------------------------------------------------------------------
-- Dirty marking. Cheap on boards that are not connected: one indexed EXISTS.
-- ---------------------------------------------------------------------------

CREATE TRIGGER IF NOT EXISTS sync_tasks_ai AFTER INSERT ON tasks
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS sync_tasks_au AFTER UPDATE ON tasks
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', NEW.id);
END;
-- Moving off a connected board. Recorded even when the target board is connected to another
-- room: the task leaves the first room either way.
CREATE TRIGGER IF NOT EXISTS sync_tasks_left AFTER UPDATE OF project_id ON tasks
WHEN OLD.project_id != NEW.project_id
 AND EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = OLD.project_id) BEGIN
    INSERT INTO sync_dirty (kind, local_id, left_from) VALUES ('task', NEW.id, OLD.project_id)
    ON CONFLICT (kind, local_id) DO UPDATE SET left_from = excluded.left_from;
END;

CREATE TRIGGER IF NOT EXISTS sync_notes_ai AFTER INSERT ON notes
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS sync_notes_au AFTER UPDATE ON notes
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS sync_note_paths_ai AFTER INSERT ON note_paths
WHEN EXISTS (SELECT 1 FROM notes n JOIN sync_rooms r ON r.project_id = n.project_id WHERE n.id = NEW.note_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', NEW.note_id);
END;
CREATE TRIGGER IF NOT EXISTS sync_note_paths_ad AFTER DELETE ON note_paths
WHEN EXISTS (SELECT 1 FROM notes n JOIN sync_rooms r ON r.project_id = n.project_id WHERE n.id = OLD.note_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('note', OLD.note_id);
END;

CREATE TRIGGER IF NOT EXISTS sync_events_ai AFTER INSERT ON events
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('event', NEW.id);
END;
-- An event changes board only by following its task or note onto it (`move_task`).
CREATE TRIGGER IF NOT EXISTS sync_events_au AFTER UPDATE OF project_id ON events
WHEN EXISTS (SELECT 1 FROM sync_rooms WHERE project_id = NEW.project_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('event', NEW.id);
END;

CREATE TRIGGER IF NOT EXISTS sync_task_repos_ai AFTER INSERT ON task_repos
WHEN EXISTS (SELECT 1 FROM tasks t JOIN sync_rooms r ON r.project_id = t.project_id WHERE t.id = NEW.task_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', NEW.task_id);
END;
CREATE TRIGGER IF NOT EXISTS sync_task_repos_ad AFTER DELETE ON task_repos
WHEN EXISTS (SELECT 1 FROM tasks t JOIN sync_rooms r ON r.project_id = t.project_id WHERE t.id = OLD.task_id) BEGIN
    INSERT OR IGNORE INTO sync_dirty (kind, local_id) VALUES ('task', OLD.task_id);
END;
