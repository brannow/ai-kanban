-- Version 18: the highest outbox seq the relay has confirmed receiving, per tag.
--
-- A relay restored from a snapshot has lost what arrived after it, while every client has
-- marked those changes sent and dropped them from its outbox -- so nobody would send them
-- again, and teammates would silently miss them. `whoami` says how far the relay has this
-- tag's changes; set against this, it tells a client the relay lost some (`check_identity`).
-- Kept apart from the outbox because that cannot say it: sent rows are deleted, and so are
-- unsent ones a disconnect drops.
CREATE TABLE IF NOT EXISTS sync_sent (
    tag  TEXT    PRIMARY KEY,
    seq  INTEGER NOT NULL
);
