# Data model

The schema is `src/core/schema.sql`, and it is the authority — this document explains
**why** it looks the way it does. `docs/plan.md` is the founding record and predates any
code; where it and this file disagree, this one wins.

Read this before changing the schema. Several columns look redundant and are not, and one
table has no consumer at all on purpose.

---

## The three entities, and why three

An agent resuming cold needs three different things, and collapsing them loses something
specific each time:

| Entity | Answers | Lifecycle |
|---|---|---|
| `tasks` | What is in flight | Moves through statuses, ends terminal |
| `events` | What happened, and **why** | Append-only, never edited |
| `notes` | What is true about this codebase | Edited in place, outlives its task |

Notes are the one people try to delete. Without them, knowledge is a time-stamped narrative
in which a superseded fact sits next to the current one with nothing marking which is which.
"The guard runs before the rewrite" and "actually the rewrite runs first" are both in the
event log, both true when written, and only one is true now. Notes are the layer that can
say which.

---

## Conventions

**Timestamps are unix epoch seconds (INTEGER, UTC).** Every read does age arithmetic — "2h
ago", "this note is six months old" — and none of it should involve parsing or timezones.

**Task IDs are global, not per-project.** Per-project numbering renders more nicely (`#1`
in a fresh project instead of `#147`) and was rejected for two reasons. An id would stop
meaning anything without its project, which breaks cross-project recall where hits from
several boards interleave. And allocating one needs `SELECT MAX()+1` inside a transaction;
multiple agent sessions write concurrently under WAL, and that races. `rowid` autoincrement
does not.

**Enumerations are CHECK constraints, not free text.** A wrong value fails at write time,
and the adapter turns the failure into a message listing the valid values. Free text would
let `Done`, `done` and `DONE` coexist and silently split every query.

---

## Tables

### `projects` / `project_paths`

One store, projects as rows. Cross-project search is a `WHERE` clause instead of a
federation problem.

A project owns **many** paths. This is the table that stops a worktree, a second clone, or
a moved folder from silently forking the board — the failure mode that matters most here,
because nothing reports it. The agent just stops finding what it wrote last week.

`projects.key` is the stable identity: the **normalized** git remote URL when there is one,
else the repo root path. Normalization collapses `git@github.com:me/repo.git` and
`https://github.com/me/repo` to one key, because cloning over SSH on one machine and HTTPS
on another is the least obvious route to a split board.

Resolution (`src/core/project.rs`) is **one upward walk**, in two passes:

1. **Markers, all the way up.** The deepest `.ai-kanban` wins. This is a pass of its own,
   not a per-level check, and that matters: an alias learned in a directory *below* a marker
   would otherwise answer first and the marker would silently never take effect. The
   realistic sequence is an agent working in a monorepo package and someone later adding a
   marker to split it out — a documented escape hatch that quietly stops working is worse
   than none.
2. **Then, per level:** a known path in `project_paths`, else a `.git` directory (keyed on
   the normalized remote if present, else the root path).

If the walk finds nothing, the starting directory becomes its own project.

Checking learned aliases at **every** level, not just the starting directory, is what stops
a subdirectory of a **non-git** project from becoming its own board. With no `.git` to mark
a root, the walk has nothing else to anchor on, so `~/notes` and `~/notes/drafts` were two
separate memories until this changed.

The creating path (`resolve_project`) and the read-only path (`find_project`) share that
walk, so they cannot disagree about which board a directory belongs to. `find_project`
exists for the session-start hook, which runs in every directory the user opens Claude Code
in and must never mint a board for a scratch folder.

`roots/list` is deliberately **not** in that list. SEP-2577 (Final) deprecates it and names
environment variables among its replacements, so the caller's path comes from
`CLAUDE_PROJECT_DIR` first and process cwd second. Roots still functions and may be
consulted opportunistically later; building the primary path on a deprecated capability
would be designing toward a removal date.

Every resolved path is recorded as an alias. That is the mitigation for split memory; the
detection is that every response states the board it resolved.

### `tasks`

`status` is one of `backlog | doing | blocked | done | archived`. There is no `next` —
priority covers it. `archived` is terminal and hidden from the default board.

**`status` is authoritative; `blocked_by` is annotation.** They may disagree:
`status = 'blocked'` with `blocked_by = NULL` is legal and means "blocked on something
outside the board". State this once or every renderer invents its own rule.

`priority` is words (`low | normal | high | urgent`), not a number. A numeric priority
forces the caller to know whether 1 means urgent or trivial — exactly the internal
knowledge the design law forbids.

`origin` (`user | agent`) is instrumentation for "does the agent file work unprompted".
**Read it as a floor, not a measurement.** It defaults to `agent` and only becomes `user`
if the model remembers to say so — and the shortcutting agent this metric is meant to
detect is precisely the one that will not bother. The count is biased in the flattering
direction. The honest signal is task *content*: did the board fill with things nobody
asked for?

`version` starts at 1 and increments on every update. It exists because `updated_at` cannot
do this job: timestamps are whole seconds, and same-second writes are routine here — it is
why every `ORDER BY` in this schema carries an `id` tie-break. A guard of
`WHERE updated_at = <what I read>` therefore passes for a second writer that committed inside
the same second, which is the exact case a guard is for. A counter has no such window. Notes
carry the same column for the same reason.

Passing it is **optional**, and the two consumers differ: the HTTP API always sends it, the
MCP agent never does. The reasoning is in `docs/http-api.md` and on `TaskPatch::expected_version`.

### `events`

Append-only. A NULL `task_id` means project-level history — a decision, a session summary —
that needs no task to hang off.

**Its reader is the board snapshot's `recent` section.** Naming the reader matters: an
append-only log nobody consumes is exactly the extra work an agent correctly skips, and it
also bounds how much narrative belongs in one entry.

`kind` conventions: `created`, `updated`, `log`, `note_added`, `note_updated`, and
`status:<new-status>` for transitions. The new status rides in the `kind` so the board can
render `#12 -> doing` without re-deriving it from the task's *current* status, which would
be wrong for every event except the latest.

One event per update call, not one per changed field. Per-field events bury the reason in
noise, and the reason is the half worth keeping.

**Note edits write an event.** `notes` holds current state only, so an in-place update with
no trace would destroy the record that a fact changed — on a board whose first priority is
history, the knowledge layer is the worst place to lose it. The event carries the previous
value. Costs one insert.

### `notes` / `note_paths`

`note_paths` associates a note with the files it is about. **Nothing consumes it yet.**

It ships anyway because the form of memory that actually gets used is contextual — the
agent opens `auth/middleware.rs` and what was learned about it last time arrives without
anyone asking. Explicit `recall` assumes the agent thinks "let me search my memory", and it
will not; it will hit a bug and start debugging. Retrofitting the association later means
re-tagging every note ever written. Cheap now, expensive later.

### FTS5

External-content tables (`content='notes'`) with triggers, so bodies are not stored twice.
`bundled` rusqlite ships FTS5 enabled — verified, not assumed, since it is a compile flag.

Three things learned building `recall`, each of which was silently wrong first:

- **`snippet(fts, -1, ...)`, never a hardcoded column.** Pinning it to the body column
  returns an *empty* snippet for anything matched on its title alone — which is the shape
  of most filed tasks. Invisible in any fixture whose tasks happen to have bodies.
- **bm25 `rank` is not comparable across tables.** Each is computed against its own corpus
  and column count, so merging three result sets and sorting by raw rank interleaves on a
  number with no shared meaning. Hits are sorted by kind first (note, then task, then
  event), then by rank within kind.
- **`created` and `note_added` events are excluded from search.** Their bodies are copies of
  the task or note title, so including them makes every entity match twice — once as
  itself, once as an event repeating its own name.

Agent input is never passed to `MATCH` raw. FTS5's syntax is a query language: `auth-loop`
is a NOT expression and a stray quote is a syntax error. Every token is quoted and ANDed,
and bare `AND`/`OR`/`NOT`/`NEAR` are dropped as noise.

---

## Response volume is part of the model

Not a rendering detail. The obvious implementation returns every task, which is correct at
ten tasks and unaffordable at two hundred — and an expensive board is one a shortcutting
agent stops calling, which is the exact failure this project exists to fix.

Measured on a year-old board (200 tasks, 40 notes, 800 events) in `tests/budget.rs`:

| Call | Cost |
|---|---|
| `board` | ~611 tokens |
| `task_add` (nothing in flight) | ~127 tokens |
| `task_add` (with in-flight work) | ~138 tokens |
| `recall`, 10 hits | ~510 tokens |
| `task_show`, 16 events | ~358 tokens |
| `board(project: "all")`, 60 boards | ~509 tokens |

Every figure above is printed by that test, not estimated.

`task_add` is the number that matters — it is paid every time the agent files a side quest,
the behaviour the project most wants to encourage. It started at 289 tokens, most of it
spent listing a dozen unrelated backlog items, and the fix was to narrow the write response
to **in-flight work plus the task that changed**. A backlog slice answers neither question
a write response should ("did it land", "what else is going on"). The backlog is still
accounted for as a count — nothing is ever dropped silently.

`board(project: "all")` is capped at 25 boards for a different reason than the rest: its
cost scales with how many repositories the user has touched all year, which is not a number
this project controls. The boards that fall off the end are the least recently active, and
the true total is still reported.

These ceilings are asserted in tests, because a widened cap costs nothing in any small
fixture and everything on a real board, with no test going red.

---

## API shape

Core returns structured data and never prose. Rendering lives in the adapter, because the
human's access to this data is meant to be an API: text in core would force a future web UI
to parse sentences back into objects it already had.

Task **and note** lookups take a `project_id` as a **parameter**, not derived from the id.
The store is global — several projects share one database — so a bare id lookup would
happily return, or splice into a board, a row belonging to a different project.

This is not theoretical for notes. `recall(project: "all")` renders note ids from other
boards, so an agent that spots a wrong note in its own search results and calls
`note_update` on it reaches the cross-project path directly. Unscoped, the edit lands on the
other board while the confirmation names this one — split memory arriving through the
knowledge layer, with nothing to signal it.

The same guard covers every cross-entity reference: `blocked_by`, a note's `task`, and a
`log` attached to a task all verify the target is on the resolved board first. Making the
caller name the board costs nothing, since every adapter resolves a project before it acts.

---

## Open problems

Named, not solved.

**Note staleness.** Nothing decides when a note stops being true, and confidently wrong
memory is worse than none. Today: `updated_at`, and `recall` shows each hit's age so the
agent can discount old claims. Automatic detection — flagging notes whose `note_paths`
files have changed substantially — is roadmap.

**Project merge.** Alias learning prevents most splits; nothing repairs one that already
happened. The mitigation is one-way until a merge path exists.

**Durability.** Everything of value is one SQLite file outside version control. Backing it
up is `cp $(ai-kanban where) somewhere`. `export`/`import` are roadmap. Multi-machine sync
is deliberately **out of scope for v1** — a decision, not an oversight.

**Adoption.** Still open, and still the load-bearing risk: a good tool surface makes
adoption possible, it does not cause it. See `docs/plan.md`.

---

## Explicitly out of scope

Task dependencies beyond `blocked_by`, sprints, estimates, burndown, assignees, WIP limits.
Board-management ceremony that serves humans managing humans.
