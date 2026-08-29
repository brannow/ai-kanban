-- Optimistic concurrency for tasks and notes.
--
-- WHY A COLUMN AND NOT `updated_at`
--
-- `updated_at` is a whole second, and same-second writes are routine here -- it is why
-- every ORDER BY in this schema carries an `id` tie-break. A guard of
-- `WHERE updated_at = <what I read>` therefore passes for a second writer that committed
-- inside the same second as the first, which is exactly the case the guard exists to catch.
-- A counter that only ever increments has no such window.
--
-- Existing rows start at 1. Any caller holding a version from before this migration has no
-- version at all, so it passes NULL and the guard is skipped -- see `update_task`.
ALTER TABLE tasks ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE notes ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
