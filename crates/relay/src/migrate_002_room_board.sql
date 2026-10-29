-- Version 2: the board a room was created from, so `room list` and `room show` can tell rooms
-- apart by something people recognize. Set once, by the join that creates the room.
ALTER TABLE rooms ADD COLUMN board TEXT;
