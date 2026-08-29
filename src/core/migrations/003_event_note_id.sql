-- Give events a note reference, so a note's history can be found and removed.
--
-- WHY
--
-- Events already carry `task_id`. Nothing carried a note id, so the only way to locate the
-- events belonging to a note was to match their body text. That made `forget_note`
-- impossible to implement correctly -- and `note_updated` bodies embed the note's PREVIOUS
-- body verbatim (so a superseded fact stays recoverable), which means overwriting a note
-- left its old content in the event log, indexed by FTS and returned by `recall`. There was
-- no way to remove anything from this store at all. See task #15.
ALTER TABLE events ADD COLUMN note_id INTEGER REFERENCES notes(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_events_note ON events(note_id);

-- Backfill what can be known for certain. `note_updated` bodies start with `note #<id> "`,
-- so the id is recoverable. The trailing space before the quote is load-bearing: without it
-- the pattern for note #1 would also match `note #12 "..."`.
UPDATE events SET note_id = (
    SELECT n.id FROM notes n
     WHERE n.project_id = events.project_id
       AND events.body LIKE 'note #' || n.id || ' "%'
) WHERE kind = 'note_updated';

-- `note_added` is deliberately NOT backfilled. Its body is the note's title with no id in
-- it, so any match would be a guess on title text -- and a wrong guess attaches one note's
-- history to another note, which is worse than a known gap.
--
-- The gap, stated plainly: a `note_added` event written before this migration survives
-- `forget_note`, carrying the note's title (not its body). Events written from here on
-- carry the id and are purged correctly. Closing it for old rows needs an id that was
-- never recorded.
