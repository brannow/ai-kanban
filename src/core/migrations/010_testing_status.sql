-- A sixth status: `testing` -- written, not yet accepted.
--
-- Why a status and not a tag, given 006 exists to keep the set fixed: 006's objection is to
-- USER-DEFINED statuses, because those have no answer to "is this open, should I pick work
-- from it" outside the user's head, and because one board's "in review" against another's
-- "reviewing" makes a store-wide recall meaningless. Neither applies here. `testing` is
-- defined once, in `Status`, with its openness (open) and its ordering (ahead of blocked and
-- backlog) settled for every consumer, and it means the same thing on every board.
--
-- What it buys is the state this board could not previously express: work whose code is
-- written but whose verification has not happened. Before this it was either `doing`, which
-- claims someone is at the keyboard, or `done`, which claims it holds -- and a session that
-- ended between the two lost the distinction, which is precisely the knowledge this project
-- exists to keep.
--
-- WHY `writable_schema` AND NOT THE DOCUMENTED TABLE REBUILD
--
-- SQLite cannot alter a CHECK constraint; the documented fix is to rebuild the table and
-- copy the rows. That fix is unsafe here. `Store::open` turns foreign keys ON, and with them
-- on, `DROP TABLE tasks` performs an implicit DELETE of every row first -- which fires
-- `task_repos`' ON DELETE CASCADE and `blocked_by`'s ON DELETE SET NULL. The rebuild would
-- silently erase every task/repo link and every blocker annotation in the store. Those
-- pragmas cannot be turned off from inside a migration either, because a migration runs in a
-- transaction and `foreign_keys` is a no-op there.
--
-- Editing the stored schema text touches no rows, so there is nothing for a cascade to
-- delete. It is narrow by construction: the UPDATE matches one table and rewrites one
-- literal list, and `writable_schema = RESET` reloads the schema on this connection so the
-- INSERT that follows in the same session sees the new constraint rather than a cached one.
PRAGMA writable_schema = ON;

UPDATE sqlite_schema
   SET sql = replace(
        sql,
        '''backlog'',''doing'',''blocked'',''done'',''archived''',
        '''backlog'',''doing'',''blocked'',''testing'',''done'',''archived''')
 WHERE type = 'table' AND name = 'tasks';

PRAGMA writable_schema = RESET;
