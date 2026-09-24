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

That key is **not** fixed over a repo's life. A repo with no remote keys on its root path.
Once it gets a remote, a fresh derivation yields `git:<remote>`, which no board has. So
**a linked worktree resolves as its main checkout**, not as a repo of its own: its `.git`
file leads through `commondir` to the main `.git`, and the walk continues from there.
Deriving from the worktree's own root forked the board in two ways. With no remote, the two
checkouts were `path:` on two different directories. When the board predated the remote,
they were `path:` versus `git:`. Worktrees are how Claude Code isolates work, so the fork
landed on an agent with no reason to suspect the board it read was empty. A submodule also
has a `.git` file, but no `commondir`; it is a separate repository and keeps its own board.

What is still open: a **separate clone** of a repo whose board was keyed before the remote
existed derives `git:` and starts a new board. The obvious repair, re-keying `path:` to
`git:` on resolve, changes the stable identity that `import` matches on. So it is a
decision of its own, not a side effect of the worktree fix.

Resolution (`src/core/project.rs`) is **one upward walk**, in two passes:

1. **Markers, all the way up.** The deepest `.ai-kanban` wins. This is a pass of its own,
   not a per-level check, and that matters: an alias learned in a directory *below* a marker
   would otherwise answer first and the marker would silently never take effect. The
   realistic sequence is an agent working in a monorepo package and someone later adding a
   marker to split it out — a documented escape hatch that quietly stops working is worse
   than none.
2. **Then, per level:** a known path in `project_paths`, else a `.git` directory (keyed on
   the normalized remote if present, else the root path).

If the walk finds nothing, the starting directory becomes its own project — **except** `$HOME`,
any directory above it, a filesystem root, or the temp dir, where resolution fails with a
message naming the ways out. Those directories sit above every project. Once one of them is
a board, it is a known path that step 2 matches for every non-git directory beneath it, so
unrelated projects silently merge into one board named after the user. That happened on a
real store: a Rust toolchain and a Swift app shared one board named after the user. The refusal
is only for this fallback. A marker or a `.git` in `$HOME` still resolves, and a board that
already claims one of these directories still resolves, so it can be read and repaired.

The set is closed on purpose. "Any directory that already contains boards" would also catch
`~/Projects`, but then whether a directory can get a board would depend on store state that
changes over time.

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

### `workstreams` / `current_workstream`

Added in migration 005. A workstream is a named slice of work inside one board — a feature,
an upgrade, a migration. It is a **grouping dimension, never an identity**.

**Why this exists.** `board()` had exactly two selection knobs, status and limit, and ordered
by recency. That is fine for one repo with one stream of work. Measured on 300 tasks across
12 workstreams, the 30 rows the board listed came from 4 workstreams picked purely by
recency, and the workstream actually being worked on contributed **1 row**; all 8 `recent`
entries came from a workstream nobody was touching. A cold agent was told, confidently and in
detail, about work that was not its own. Cost was never the problem — that board was ~583
tokens, well inside budget. *Selection* was.

**Why not separate boards**, which is the intuitive fix and was rejected twice:

- `blocked_by` is same-project by design (`task.rs` rejects a cross-project blocker), so
  splitting features onto their own boards breaks cross-feature blocking — usually the first
  thing anyone actually wants.
- Project resolution is path-derived, and a feature is not a directory. A finished feature's
  board becomes unreachable.
- `merge` reparents rows and deletes the source with no provenance, so merging a finished
  feature back produces exactly the undifferentiated pile you started from.

**Why a table and not `tags`.** The directory line needs `name, COUNT(*)` per workstream on
every board call. `tags` is a comma-joined TEXT column and nothing enumerates it; answering
that from joined text means splitting every row in the project. Tags remain what they are —
freeform, multi-valued, human-facing labels. A workstream is single-valued, enumerable and
closable. Different shape, different job, and they are **not** the same feature.

**`closed_at` is about the directory, not the tasks.** Closing hides a workstream from the
listing; its tasks stay on the board and stay counted. Task-level collapsing is already
`status`'s job — done work occupies zero rows. A site maintained for three years would
otherwise list forty dead workstreams on every session start.

**Unscoped tasks (`workstream_id IS NULL`) are in scope always.** A task with no workstream is
general project work: it belongs to every view, not to none. This is also what makes the
feature safe to turn on — every board that predates 005 is entirely unscoped, and scoping such
a board strictly would blank it.

**The sticky pointer is its own table, and `workstream_id` is not in `TASK_COLS`.** Both
choices exist to avoid raising `MIN_READABLE_VERSION`. Read paths select explicit column
lists, so a new column in one of them makes a store this binary has not migrated unreadable —
and the `SessionStart` hook opens read-only and must never migrate. The pointer went into
`current_workstream` so `row_to_project`'s list would not change. Cost: one extra table.
Bought: the hook keeps working through the upgrade instead of going silent for a session.
See note #13 for why the "adding a column is backward compatible" intuition is backwards.

> **Keeping a column out of `TASK_COLS` is not sufficient, and assuming it was shipped a
> bug.** Naming a column *anywhere* in SQL requires it to exist — `WHERE` and `ORDER BY`
> included. The first version of this feature emitted its scope filter unconditionally, so
> every board query against a pre-005 store failed; because every hook path is `.ok()?`,
> nothing errored and the `SessionStart` board simply vanished. That is the exact regression
> all of the above exists to prevent, reintroduced one clause further down.
>
> So when nothing is scoped, the generated SQL must not mention `workstream_id` at all. The
> unscoped filter is `(?2 IS NULL)` rather than a bare `1`, which keeps `?2` referenced so
> the parameter numbering is identical in both arms — drop the only use of `?2` and SQLite
> reports the statement as taking one parameter while the caller binds two.
>
> `tests/workstream.rs` drops the index, the tables and the column, then asserts an unscoped
> board and the hook still render. Any future migration that adds a column *without* raising
> `MIN_READABLE_VERSION` needs the same test, or the claim is untested folklore.

**Ordering puts the active workstream ahead of status.** This is the feature, not a detail.
Ordered by status alone, fifty unscoped legacy tickets crowd three active-workstream tasks
straight out of a 30-row limit — shipping the original failure with a new column that was
supposed to fix it. The trade, stated because it is real: an unscoped `doing` task now sorts
below an active-workstream `backlog` one. Displaced work is counted, never dropped.

### `repos` / `board_repos` / `task_repos`, and `tasks.external_ref`

Added in migrations 007 and 008. A **repo** is a local checkout work happens in. A ticket names
the repos it touches, a repo carries many tickets, and a repo can be on several boards.

**Boards own repos, rather than every repo being its own board.** A board is a body of work — a
customer, a product, a tracker's project — and it spans several repositories; a ticket in it
touches some of them. With one board per repo, a ticket touching two has to live on one, and an
agent opening the other never sees it. A repository can also serve more than one body of work —
a shared library, a monorepo two projects deploy from — so repos are global and tied to boards
through `board_repos`, many-to-many. 007 gave each repo one board; 008 replaced that the first
time real use put one repo on two projects.

**Exactly one home.** What cannot be shared is where a folder resolves: an agent opening a
directory has to land on one board, the same one every time, or the folder reads a different
memory on different days. So each repo has a **home** (`repos.home_project_id`) — the board its
root's `project_paths` alias points at, always one of its boards. It starts as the first board
the repo is added to. Adding the same checkout to another board *attaches* it and leaves the
home alone; `set_repo_home` moves it, re-pointing the root alias and every subdirectory the old
home learned (left behind, a learned `src/` alias would keep answering for the old home). When
the home lets a repo go, the home passes to the earliest remaining board. "Most recently active
board" was rejected: it makes resolution depend on the clock.

**`repos.path` and `repos.name` are unique across the store.** The path because one directory
resolves to one board; the name because a repo is one thing seen from several boards, and
`eee-api` must mean the same checkout on each. Registering a *new* checkout in a directory
another board already claims — at that path *or below it* — is refused with the owner named.
Below matters: resolution checks the deepest alias first, so another board's `src/` alias would
keep answering and the registration would silently not take effect. `merge` is the repair when
both boards are the same work.

**Removing a repo from its last board leaves its path alias in place.** Taking it out of the
menu says it is no longer part of this work, not that its history belongs elsewhere. Dropping
the alias would send the next session in that folder to a brand-new empty board.

**Boards can be created by name** (`create_board`, key `board:<name>`), for a project that is
not one directory. Such a board has no path of its own; agents reach it through the repos it is
home to. A name another board has is refused, because two boards with one name is what a split
looks like.

**A ticket can move between boards** (`move_task`). Task ids are global, so it keeps its id, and
its events and the notes written on it move with it. What only means something on the old board
— its blocker, its workstream, a repo the new board lacks, other tasks' `blocked_by` pointing at
it — is dropped, and the `moved` event says what was left behind.

**A ticket with no repo is flagged, not blocked.** `status` is authoritative and carries
behaviour — the argument under `tags` below. A status forced by a rule is one the agent did not
set and cannot explain, and `blocked` would stop meaning one thing. The board line says
`no repo set` instead: only on boards that have repos, and only on open work, because a line
that always says the same thing is one a reader learns to skip.

**`external_ref` is the issue in an outside tracker a task mirrors** — `48213`, `PROJ-123`. 007
added it as `planio INTEGER`, naming one vendor in the schema; 012 made it tracker-agnostic
TEXT, because a Jira key or anything else that is not a bare number could not be stored. Which
tracker it is, and where its issues live, is the person's setting (`AI_KANBAN_REF_URL`), not the
store's. 012 is a new migration rather than an edit to 007 because 007 had already run on the
stores it shipped to, and `migrate::run` skips every migration at or below a store's version.

TEXT gives up what the INTEGER bought — `48213`, `#48213` and a pasted URL coexisting as three
spellings of one ticket — so `core::task::check_external_ref` takes that job over for both
adapters: blank clears, a leading `#` is dropped, whitespace and URLs are refused. It is indexed
in `tasks_fts` so `recall 48213` finds the ticket; the FTS table is dropped and rebuilt, as in
006. Exports written before 012 carry `"planio": 48213` and still import.

Neither is in `TASK_COLS`, following 005 and 006, so `MIN_READABLE_VERSION` stays at 2. The
reads that name them (`links_for`, `repo_count`, `task_repos`, `task_external_ref`) degrade to "no
repos" on a store the read-only hook finds unmigrated. `tests/repos.rs` drops the tables and
the column and asserts the board and the hook still render. `links_for` reads the ref and the
repos in separate tolerant queries, so a store between 007 and 012 loses only the ref.

Unlike tags, repos and the external ref **are on the board line**. Tags carry nothing an agent
acts on; which checkouts a ticket lives in and which ticket it is are what an agent needs to
pick the work up at all. Measured at its worst — two repos and a ref on every listed row — in
`tests/budget.rs`.

**Repos are registered from any surface**: the web UI's repos menu, `repo_add`, or
`ai-kanban repo add`. All three call `Store::add_repo`, which is where the name is normalized
and where a path another board already claims is refused — the guard that makes it safe for an
agent to register one. What stays the person's call is *moving* a repo's home
(`set_repo_home`), since that is what changes where an existing folder resolves.

### `tasks`

`status` is one of `backlog | doing | blocked | testing | done | archived`. There is no
`next` — priority covers it. `archived` is terminal and hidden from the default board.

`testing` is work that is written but not accepted: the code exists, and a run, a review or a
person still has to say it holds. It counts as **open**, and that is the whole point — a task
parked there is unfinished work, and calling it done is how "built but never verified" dies
with the session that built it. It ranks just behind `doing` on the agent's board, ahead of
`blocked` and `backlog`, because it is the closest thing on the board to finished and the
easiest thing for a returning agent to close out.

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

**`tags` — the escape valve that keeps the status set fixed.**

People ask for custom board columns. They cannot have them, and the reason is not
conservatism: every status carries BEHAVIOUR, not just a label. `is_open()` decides what
"8 open" means on the board an agent reads cold, `board_rank` decides ordering, `blocked` is
tied to `blocked_by`, `archived` is terminal. A user-defined status has no answer to "is this
open, should I pick work from it" except in the user's head — which inverts the design law:
the agent would need knowledge of the USER'S configuration, worse than needing ours. And
since the store is global and recall crosses projects, one board's "in review" against
another's "reviewing" quietly makes cross-project search meaningless.

`testing` (migration 010) is not a counterexample. It is defined once, in `Status`, with its
openness and its ordering settled for every consumer, and it means the same thing on every
board — which is exactly what a user-defined column cannot offer. The bar for a new status is
that behaviour, not the label.

Tags carry no semantics an agent must honour, which is exactly what makes them safe. A person
gets "in review", "waiting-on-vendor", "frontend" without inventing workflow states.

Consequences worth knowing:

- **Not in `TASK_COLS`**, following `workstream_id` — that is what keeps
  `MIN_READABLE_VERSION` where it is. `task_tags` and `tags_for` read it separately, and both
  are on paths that have already migrated.
- **Shown in `task_show` and the web UI, never on the board line.** The board listing is the
  most expensive space in the product and is paid for on every `task_add`; tags buy an agent
  nothing there. The asymmetry is deliberate: an agent can set a tag it will not see on the
  board, which is correct, because tags are for the person's filtering.
- **Not the storage for workstreams.** A workstream must be ENUMERABLE (the directory needs
  name + count) and single-valued per task. A comma-joined column serves neither.

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

**Adding a column to an external-content FTS table means dropping and rebuilding it.** The
column list is fixed at creation, so migration 006 (`tasks.tags`) had to drop the table and
all three sync triggers, recreate both, and `INSERT INTO tasks_fts(tasks_fts) VALUES('rebuild')`.
Forgetting the rebuild does not error — it leaves an empty index, which is indistinguishable
from "nothing matched". `tests/migrate.rs::adoption_leaves_the_search_index_intact` is the
test that catches it; the rest of the suite stays green either way.

Tags are **indexed, not filterable**. `recall`'s contract is "plain words — no search syntax
needed", so a `tag:frontend` operator would be a second query language beside FTS5's and
exactly the internal knowledge the design law forbids. Indexed, a tag is found by typing it.

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

**Note staleness — partly answered.** Nothing decides when a note stops being *true*, and
confidently wrong memory is worse than none. Two signals now exist, and the gap between
them is the point:

- `updated_at`, with each `recall` hit's age, so an old claim can be discounted.
- **Deleted subjects.** A note naming a file that is no longer in the repo is flagged on
  recall. `src/core/staleness.rs` carries the reasoning, including three approaches that
  were rejected — git last-commit time (needs a subprocess, which `project.rs` deliberately
  avoids), file mtime (a fresh clone rewrites every one, flagging the whole board), and a
  stored content hash (accurately answers "did the file change", which is not the same
  question as "is the note wrong", and fires on every reformat).

  The check is calibrated: it stays silent unless at least one of the paths it is looking
  at resolves. Note paths are repo-relative while the store is global, so on a machine
  where the project was never checked out *nothing* resolves — and flagging every note
  there would be noise that trains the reader to ignore the flag, destroying the true
  positives with it.

What is still open is the harder half: a note can be about code that still exists and still
be wrong. Nothing detects that, and file-watching does not, either.

**Project merge — solved.** `ai-kanban merge <keep> <gone>` reparents one board onto
another and deletes the empty one; see `docs/architecture.md`. Merging boards from two
*different stores* remains open (#20) and is a different problem: ids collide meaninglessly.

**Durability — solved for one machine.** `ai-kanban backup` writes one consistent file, and
`export`/`import` move boards as JSON. Multi-machine sync is deliberately **out of scope for
v1** — a decision, not an oversight.

**Adoption.** Still open, and still the load-bearing risk: a good tool surface makes
adoption possible, it does not cause it. See `docs/plan.md`.

---

## Explicitly out of scope

Task dependencies beyond `blocked_by`, sprints, estimates, burndown, assignees, WIP limits.
Board-management ceremony that serves humans managing humans.

---

## `todos` and `todo_rev`: dormant tables

Migration 011 creates them and nothing reads or writes them. They held a personal to-do list
for the human, reachable only from the web UI, that was built on the `mb-fork` branch and taken
out again before it reached main: it makes no agent work better, which is the test in
`docs/vision.md`, and it would have been the one documented exception to "every mutation
writes an event". It may come back later as an opt-in.

**Why the tables stay.** 011 had already run on the stores the branch shipped to, and
migrations are append-only: removing 011, or giving its number to something else, would leave
those stores at version 11 and silently skip whatever took its place. Dropping the tables in a
later migration would delete the to-do items people wrote on that branch. Empty tables cost
nothing, and an opt-in would start from them. If the feature is ever declared dead for good,
drop them in a new migration -- never by editing 011.
