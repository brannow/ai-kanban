-- Version 5: what tells a client this is another relay, and which tags are spoken for.
-- (docs/sync.md, "Moving to another relay".)
--
-- instance: made once with the database and never changed, so a copy of the database on a
-- new host is the same relay. A client remembers the instance it last synced with: the same
-- one with a shorter log was restored from a snapshot; another one moved or was set up anew,
-- and the client sends everything it holds again.
CREATE TABLE IF NOT EXISTS instance (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    value  TEXT    NOT NULL
);
INSERT OR IGNORE INTO instance (id, value) VALUES (1, lower(hex(randomblob(16))));

-- seen_tags: every tag a change in any room carries, as the creator of what it is about or as
-- the author of the change. A generated tag never takes one: after a move, an absent member's
-- work is in the room before they are, and a new member given their tag would read it as
-- their own. A member claiming their own tag back may take it (see `name_key`).
CREATE TABLE IF NOT EXISTS seen_tags (
    tag  TEXT  PRIMARY KEY
);
INSERT OR IGNORE INTO seen_tags (tag) SELECT DISTINCT entity_origin FROM changes;
INSERT OR IGNORE INTO seen_tags (tag) SELECT DISTINCT subject_origin FROM changes WHERE subject_origin IS NOT NULL;
INSERT OR IGNORE INTO seen_tags (tag) SELECT DISTINCT tag FROM changes;
