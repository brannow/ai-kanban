-- Version 20: which relay this store last synced with, and which rooms must be joined again.
-- (docs/sync.md, "Moving to another relay".)
--
-- relay_instance: the relay's own id (`hello`), made once with its database. Another one than
-- this store last synced with is a relay that moved or was set up anew: everything held is
-- sent there again. The same one at a new URL is the same relay, copied to a new host.
-- NULL until the first sync with a relay that names one.
ALTER TABLE sync_node ADD COLUMN relay_instance TEXT;

-- rejoin: the room is on a relay where this store is not a member yet. Joined (and created, if
-- nobody is back yet) before anything is pushed, which would otherwise be refused.
ALTER TABLE sync_rooms ADD COLUMN rejoin INTEGER NOT NULL DEFAULT 0;

-- own_name: the name this machine goes by on the relay, as the relay last said. A new relay's
-- key is named with it when the plugin settings give no name, so a move does not turn "Benny"
-- into a generated "clever-crane".
ALTER TABLE sync_node ADD COLUMN own_name TEXT;

-- sync_names: the last name each tag went by, kept when it leaves a member list. After a move
-- a teammate not back yet is missing from the new relay's list, and their tasks would show a
-- bare tag instead of who they are. Filled from the member lists already recorded.
CREATE TABLE IF NOT EXISTS sync_names (
    tag   TEXT  PRIMARY KEY,
    name  TEXT  NOT NULL
);
INSERT OR IGNORE INTO sync_names (tag, name)
    SELECT json_extract(m.value, '$.tag'), json_extract(m.value, '$.user')
      FROM sync_room_status s, json_each(s.members) m
     WHERE s.members IS NOT NULL
       AND json_extract(m.value, '$.tag') IS NOT NULL AND json_extract(m.value, '$.user') IS NOT NULL;

-- sync_left_relays: the instances this store moved away from. A process still holding the old
-- relay's URL and key -- a session started before the move -- would otherwise read the old
-- relay as a new move and send everything back there. Refused unless the saved remote is that
-- relay again, which only a person deciding to go back makes it.
CREATE TABLE IF NOT EXISTS sync_left_relays (
    instance  TEXT     PRIMARY KEY,
    left_at   INTEGER  NOT NULL
);
