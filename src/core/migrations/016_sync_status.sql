-- What the board header and `room status` say about a shared board: when it last synced, why
-- the last attempt failed, who is in the room, and whether sync is paused for this store.
--
-- New tables rather than ALTER TABLE ... ADD COLUMN, which cannot run twice: like 013-015 this
-- is safe to apply to a store that already has it, which is what the migrate tests do when they
-- fake an older store by resetting `user_version` (note #178). Side tables also leave every
-- column list the hook reads untouched (notes #13, #71).

CREATE TABLE IF NOT EXISTS sync_room_status (
    project_id  INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    last_ok_at  INTEGER,
    -- Set when the last attempt failed (offline, key rejected), cleared by the next success.
    last_error  TEXT,
    -- The room's members as the relay last reported them: [{"tag": "v1", "user": "volker"}].
    members     TEXT    NOT NULL DEFAULT '[]'
);

-- Why sync is stopped for the whole store, if it is: a protocol the relay does not speak, a
-- key for another tag, or counters behind the relay's (a restored backup). Local work goes on.
CREATE TABLE IF NOT EXISTS sync_pause (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    reason  TEXT    NOT NULL
);
