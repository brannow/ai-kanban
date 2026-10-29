-- Version 3: the relay issues keys; people name themselves.
--
-- A key is issued with only a label -- the admin's note of who it was given to, never shown
-- to the team -- and no tag. Its tag is fixed the first time it is used: by the name its client
-- sends, or by a generated one. The name belongs to the tag, so it survives a key rotation, and
-- the client may change it at any time. Names may repeat; the tag tells two people apart.
--
-- So there are no users any more. Each existing tag keeps its user's name, each existing key
-- gets that name as its label, and the keys of disabled users are revoked here: nothing checks
-- `users.disabled_at` after this, and their keys must not quietly start working again.
--
-- Rebuilt rather than altered, since SQLite cannot drop a NOT NULL or a column with a foreign
-- key: create the new table, copy, drop the old, rename the new (in that order, so the foreign
-- keys of `members` and `keys` keep naming `tags`). `init` turns foreign keys off around this
-- and checks them afterwards.

UPDATE keys SET revoked_at = CAST(strftime('%s', 'now') AS INTEGER)
 WHERE revoked_at IS NULL
   AND tag IN (SELECT t.tag FROM tags t JOIN users u ON u.id = t.user_id WHERE u.disabled_at IS NOT NULL);

CREATE TABLE tags_new (
    tag         TEXT    PRIMARY KEY,
    name        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL
);
INSERT INTO tags_new (tag, name, created_at)
    SELECT t.tag, u.name, t.created_at FROM tags t JOIN users u ON u.id = t.user_id;

CREATE TABLE keys_new (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- NULL until the key is first used.
    tag          TEXT    REFERENCES tags(tag),
    label        TEXT    NOT NULL,
    hash         TEXT    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);
-- Ids kept: `key revoke 3` must still mean the same key.
INSERT INTO keys_new (id, tag, label, hash, created_at, last_used_at, revoked_at)
    SELECT k.id, k.tag, COALESCE(NULLIF(k.label, ''), u.name), k.hash, k.created_at, k.last_used_at, k.revoked_at
      FROM keys k JOIN tags t ON t.tag = k.tag JOIN users u ON u.id = t.user_id;

DROP TABLE keys;
DROP TABLE tags;
DROP TABLE users;
ALTER TABLE tags_new RENAME TO tags;
ALTER TABLE keys_new RENAME TO keys;
CREATE UNIQUE INDEX keys_live_tag ON keys(tag) WHERE revoked_at IS NULL;
