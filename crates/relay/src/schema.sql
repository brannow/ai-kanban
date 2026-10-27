-- ai-kanban relay -- server database, version 1. See `src/server/store.rs`.
--
-- FROZEN once shipped, like core's migrations: add a new entry to MIGRATIONS instead.
--
-- Timestamps are unix epoch seconds, as everywhere else in ai-kanban.

CREATE TABLE users (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL,
    disabled_at INTEGER
);

-- An origin tag belongs to one user forever. Handles published under it (`#122-v1`) outlive
-- every key that ever carried it, so it is never reassigned.
CREATE TABLE tags (
    tag         TEXT    PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    created_at  INTEGER NOT NULL
);

-- Only the hash is kept. Several keys may exist per tag over time (rotation); at most one is
-- live.
CREATE TABLE keys (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    tag          TEXT    NOT NULL REFERENCES tags(tag),
    label        TEXT    NOT NULL DEFAULT '',
    hash         TEXT    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);
CREATE UNIQUE INDEX keys_live_tag ON keys(tag) WHERE revoked_at IS NULL;

CREATE TABLE rooms (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL,
    created_by  TEXT    NOT NULL
);

-- Who is connected. `left_at` instead of a delete, so the empty-room rule can measure how long
-- a room has been abandoned.
CREATE TABLE members (
    room_id     INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    tag         TEXT    NOT NULL REFERENCES tags(tag),
    joined_at   INTEGER NOT NULL,
    left_at     INTEGER,
    PRIMARY KEY (room_id, tag)
);

-- The log. `seq` is AUTOINCREMENT so it never goes back, even after a forget deletes the newest
-- rows: it is every client's resume cursor. `body` is the change as sent, unread by the server;
-- the entity and subject columns are copied out of it only so forget can find its rows.
CREATE TABLE changes (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id        INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    cid            TEXT    NOT NULL UNIQUE,
    author         TEXT    NOT NULL,
    tag            TEXT    NOT NULL,
    received_at    INTEGER NOT NULL,
    entity_kind    TEXT    NOT NULL,
    entity_origin  TEXT    NOT NULL,
    entity_id      INTEGER NOT NULL,
    subject_kind   TEXT,
    subject_origin TEXT,
    subject_id     INTEGER,
    body           TEXT    NOT NULL,
    body_hash      TEXT    NOT NULL
);
CREATE INDEX changes_room ON changes(room_id, seq);
CREATE INDEX changes_entity ON changes(room_id, entity_kind, entity_origin, entity_id);
CREATE INDEX changes_subject ON changes(room_id, subject_kind, subject_origin, subject_id);

-- Kept forever, so a member who was offline during a forget cannot push the content back.
CREATE TABLE forgotten (
    room_id     INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL,
    origin      TEXT    NOT NULL,
    id          INTEGER NOT NULL,
    at          INTEGER NOT NULL,
    PRIMARY KEY (room_id, kind, origin, id)
);

-- The highest counters accepted per tag: `outbox` (change ids) and one per entity kind for
-- entities that tag created. A client restored from a backup has lower counters and would
-- reissue ids the room already holds; `whoami` hands these out so it can notice.
CREATE TABLE high_water (
    tag         TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    value       INTEGER NOT NULL,
    PRIMARY KEY (tag, kind)
);
