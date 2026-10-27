-- What the board header and `room status` say about a shared board: when it last synced, why
-- the last attempt failed, who is in the room, and whether sync is paused for this store.
--
-- Columns on side tables only (`sync_rooms`, `sync_node`), which no read query of the hook path
-- selects by list, so MIN_READABLE_VERSION is untouched (notes #13, #71).

ALTER TABLE sync_rooms ADD COLUMN last_ok_at INTEGER;
-- Set when the last attempt failed (offline, key rejected), cleared by the next success.
ALTER TABLE sync_rooms ADD COLUMN last_error TEXT;
-- The room's members as the relay last reported them: [{"tag": "v1", "user": "volker"}, ...].
ALTER TABLE sync_rooms ADD COLUMN members TEXT NOT NULL DEFAULT '[]';
-- Why sync is stopped for the whole store, if it is: a protocol the relay does not speak, a
-- key for another tag, or counters behind the relay's (a restored backup). Local work goes on.
ALTER TABLE sync_node ADD COLUMN paused TEXT;
