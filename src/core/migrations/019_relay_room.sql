-- Version 19: which room on the relay a board is in, by the relay's id for it, not only its name.
--
-- A room deleted on the relay and opened again under the same name is another room. Bound by
-- name alone, a board that was in the old one kept pulling and took in the new room's tasks,
-- silently (tests/remote_sync.rs). The relay never reuses a room id, so comparing it on every
-- pull tells the rooms apart. NULL for a board connected before this: it takes the id of the
-- room it pulls from next.
ALTER TABLE sync_rooms ADD COLUMN relay_room INTEGER;
