# The HTTP API

The second consumer. `docs/vision.md` names it: the agent gets MCP, the human gets an HTTP
API and a web UI on top of it. This document defines that API's shape and the decisions
behind it, before any of it is built.

Status: **built.** `ai-kanban serve` runs it; the page is embedded in the binary.

Everything it depended on landed first: schema migrations (#9), the row-version concurrency
guard (#10), delete semantics (#11), project resolution over HTTP (#12) and the rule that
every mutation writes an event (#13).

## What makes this consumer different

Every difference below follows from one fact: **the human is a second writer, and the server
does not own the writes.**

An agent's MCP server is a separate process, spawned per session, writing the same SQLite
file. The HTTP server never sees those writes. It cannot emit a change event when they
happen, because nothing tells it they happened.

That single fact produces most of this document: it is why there is a change cursor, why
live updates are polled rather than pushed from the write path, and why every mutation needs
a concurrency guard that the MCP surface never needed.

The other differences are smaller but real:

| | MCP (agent) | HTTP (human) |
|---|---|---|
| Response volume | a hard budget — an expensive tool gets routed around | a scrollable page |
| Project context | ambient, from `cwd` | must be explicit, always |
| Rendering | prose, from `render` | structured JSON, rendered by the browser |
| Writes | short-lived, seconds apart | a form open for minutes |
| Actor | `agent` | `user`, and never client-supplied |

## Principles

**1. JSON only. Never `render`'s prose.**
Core returns structs precisely so this consumer does not have to parse sentences back into
objects — `docs/architecture.md` states the rule and this is the case it was written for.
Concretely: send raw unix timestamps plus the server's `now`, and let the browser compute
"2h ago". Sending the rendered string means the UI cannot tick a relative time without
refetching the whole board.

**2. The project is always in the path. There is no "current".**
`Scope::Current` resolves from the MCP server's `cwd`, captured once at startup. HTTP has no
per-request working directory, so `Current` is not merely inconvenient here, it is
meaningless. Every endpoint names its project. (Task #12.)

**3. The HTTP layer gets its own query type. `BoardQuery` is not widened.**
`BOARD_LIMIT = 30` exists because response volume is the agent's budget, and `tests/budget.rs`
asserts it. Raising it to fill a web page would leave that test asserting the web UI's page
size — which `CLAUDE.md` already names as the way the board quietly becomes too expensive to
use. Two consumers, two limits, one core.

**4. Paginate keyset, not offset.**
`ORDER BY updated_at DESC, id DESC` with `WHERE (updated_at, id) < (cursor)`. Offset paging
over a table an agent is actively mutating duplicates and skips rows. The tie-break column
this needs already exists on every ordered query in the codebase.

**5. Bind `127.0.0.1`. Not configurable in v1.**
The store is global: every project the user has ever opened, and every note about their code.
There is no auth, so there must be no remote surface. Written down as a decision so it is not
later "improved" into `0.0.0.0` by someone who reads the bind address as an oversight.

## Resources

```
GET    /api/meta                              enum values, versions — cacheable
GET    /api/projects                          summaries, keyset
GET    /api/projects/{p}                      project + its path aliases
GET    /api/projects/{p}/board                one request paints the board
GET    /api/projects/{p}/tasks                ?status= &cursor= &limit=
POST   /api/projects/{p}/tasks                -> 201, Task
GET    /api/projects/{p}/tasks/{t}            TaskDetail, ETag
PATCH  /api/projects/{p}/tasks/{t}            If-Match required
GET    /api/projects/{p}/notes                ?path= &tag= &cursor= &limit=
POST   /api/projects/{p}/notes                -> 201, Note
GET    /api/projects/{p}/notes/{n}            ETag
PATCH  /api/projects/{p}/notes/{n}            If-Match required
GET    /api/projects/{p}/events               ?cursor= &limit=   the history
GET    /api/projects/{p}/repos                the repos menu: repos + ticket counts
POST   /api/projects/{p}/repos                {path, name?} -> 201, Repo
PATCH  /api/projects/{p}/repos/{r}            {name} -- rename only
DELETE /api/projects/{p}/repos/{r}            off this board and this board's tickets
PUT    /api/projects/{p}/repos/{r}/home       its folder opens on this board from now on
POST   /api/projects                          {name} -> 201, a board created by name
POST   /api/projects/{p}/tasks/{t}/move       {to, log} -- If-Match required
GET    /api/repos                             every repo, with the board its folder opens on
GET    /api/all/board                         All Projects: every board's tickets, one set of columns
GET    /api/all/tasks                         ?status= &cursor= -- paging an All Projects column
GET    /api/recall                            ?q= &project= &limit=
GET    /api/stream                            SSE, live updates
```

`{p}` accepts a project id or a project key. Names are not accepted: they are not unique.

### `/api/meta` — so the UI never hardcodes an enum

Returns the statuses, task types, priorities and actors, each with its display order
(`Status::column_rank`, `Priority::rank`).

The web board's columns run backlog, blocked, doing, done, archived — doing in the middle.
That is `column_rank`, deliberately not the agent's `board_rank`, which lists in-flight work
first because it decides which rows survive a capped text board. A person sees every column
at once, so the column order is free to follow how they read the board.

This is the design law applied to consumer #2. A web UI that hardcodes
`["backlog","doing","blocked"]` needs internal knowledge of the schema, and drifts silently
the day a status is added. It builds its columns and its dropdowns from this document
instead.

**It deliberately carries no cursor.** Enum values and the build version never change while
the process runs; the schema version changes only on migration. Mixing a
constantly-changing cursor into the same document would make a UI that needs the cursor
refetch the enum table forever.

The rule: **the cursor rides on reads that snapshot state** — `/board`, and any list page —
and on the stream. Never on cacheable metadata. A read carries the cursor so the stream can
tell that client whether what it is showing has gone stale.

### `/api/projects/{p}/board`

```jsonc
{
  "project": { ... },
  "now": 1756400000,
  "cursor": 412,                 // the events.id this read was taken at
  "counts":  [ { "status": "backlog", "count": 12 }, { "status": "doing", "count": 2 } ],
  // An ARRAY, already in column order -- the client renders left to right without knowing
  // the ordering rule. `total` is every task in that status; `tasks` is the first page.
  "columns": [
    { "status": "backlog", "total": 75, "tasks": [ ... ], "next": "1756399000:31" },
    { "status": "doing",   "total": 2,  "tasks": [ ... ], "next": null }
  ],
  "workstream":  { ... },        // what the board is scoped to, or null
  "workstreams": [ ... ],        // everything selectable
  // Status of every task a listed card is blocked by. A card carries only the blocker's
  // id, so without this the board reads "blocked by #16" for as long as the row exists --
  // including long after #16 was finished.
  "blocker_status": [ { "id": 16, "status": "done" } ],
  // Tags for the listed tasks, keyed by id, and only for tasks that have any. Sent
  // alongside the rows rather than on them because `tags` is out of TASK_COLS, so a listed
  // task does not carry it. `/tasks` returns the same field for the rows in its page --
  // without that, an expanded column would show tags on the first 50 cards and none after.
  "task_tags": { "68": ["frontend", "in review"] }
}
```

Not a `BoardSnapshot`. The agent's snapshot is a capped list with an `omitted` count, which
is the right shape for something that must fit in a token budget and the wrong shape for a
board with columns you scroll. One request paints the whole board; each column then pages
independently against `/tasks?status=`.

**`total` and `tasks.length` differ, and the client must show that they differ.** A column
that lists 50 of 75 under a header reading `75` is a task that cannot be reached: not by
scrolling, and not by browser find, because it was never in the document. Whatever replaces
the agent snapshot's `omitted` count here has to be visible in the same way -- dropping the
count did not remove the cap, it only removed the evidence of it. This was a real bug: the
paging half of this design was specified here and never built client-side, so the cap stayed
silent for as long as the UI existed.

**The whole read runs in one transaction** (`BEGIN DEFERRED`). `counts` and the per-column
pages are separate queries, and an agent writing between them would render `backlog (12)`
above thirteen cards. A read transaction costs nothing here and removes the class entirely.

**`/tasks` is scoped to the project's current workstream, exactly as `board` is.** It is the
endpoint a column pages against, and `counts` comes from `status_counts_in`, which is already
scoped -- so an unscoped page two would put more cards in a column than its own header
counted. The scope is read server-side rather than passed as a query parameter: it can change
between the board load and the "load more" click, and a client replaying a name it captured
earlier would silently splice two slices of the board together.

### `/api/recall`

```jsonc
{
  "now": 1756400000,
  "scope": "all projects",       // or the project name, when `project=` was given
  "hits": [ { "kind": "task", "id": 68, "task_id": 68, "project": "sbb", "title": "...",
              "snippet": "... >>news<< ...", "ts": 1756399000, "score": -1.8,
              "status": "backlog" } ],
  "omitted": 35,                 // matches the cap left out
  "available": { "notes": 67, "tasks": 196, "events": 508, "projects": 5 }
}
```

**`omitted` is counted, not derived from `hits.length`.** The cap is applied twice — each of
the three searches (notes, tasks, events) takes `limit` rows, and the merged list is then
truncated to `limit` again — so the number of hits returned is a lower bound on the matches
and nothing more. A search returning 20 of 55 matches would otherwise report 0 omitted, which
is the failure this field exists to prevent: a capped result that looks complete sends the
reader away believing the store holds nothing else.

`hits[].id` is the id of the matched thing in its own id space; `task_id` is the task to
follow, and is null for a note or for an event with no task. A client making hits clickable
must key on `task_id`, not `id`. `project` is a display NAME — `{p}` in every other route
takes an id or a key, so following a hit to another board needs a name-to-id lookup from
`/api/projects`.

### Writes

Everything a human sends is `origin: "user"` and `actor: "user"`, **hardcoded in the adapter
and not accepted from the client.**

Not because "the web UI is a human" — because `tasks.origin` and `events.actor` are the
instrumentation for priority #2 (does the agent file work unprompted), and `docs/adoption.md`
already describes that number as a floor rather than a measurement. A client-supplied actor
lets the floor be inflated, which makes the one number the project uses to judge itself
worthless.

**`POST /api/projects/{p}/notes` takes `paths`, and the UI must send them.** `note_paths` is
what the PostToolUse hook reads to surface notes contextually. A note filed through the web UI
with no paths attached is a note the agent will never be handed while editing the file it is
about — it exists for the agent's benefit and never reaches it. The UI should default `paths`
to whatever file the user was looking at when they filed it. This is the difference between
the web UI feeding the agent's memory and merely reading it.

`POST /api/projects/{p}/events` is **not** in v1. The `log` tool exists so an agent can record
a project-level decision without inventing a task to hang it on. A human typing free text
into the history table is not that, and `docs/vision.md` names "not a chat log" as a non-goal
aimed at exactly this. The human's writes are tasks and notes.

### Concurrency: `ETag` and `If-Match`

`GET` of a single task or note returns `ETag: "<version>"`. `PATCH` requires `If-Match` and
answers `412 Precondition Failed` when it does not match, with the current representation in
the body so the UI can show what changed.

This is required from v1, not added later. A browser form sits open for minutes while an
agent moves the same task; without the guard one write silently clobbers the other, and the
event log records *both*, so the history reads as though the board contradicted itself. On a
project whose first priority is history, that is the worst kind of bug — silent, and it
corrupts the record rather than the state.

**The token is the row's `version`** — a counter that increments on every update, added by
migration 002. Not `updated_at`: timestamps are whole seconds and same-second collisions are
routine here (it is why every `ORDER BY` carries an `id` tie-break), so a writer committing
inside the same second would pass an `updated_at` guard. A counter has no such window.

Core already enforces this (`TaskPatch::expected_version`, `NotePatch::expected_version`) and
returns `Error::Conflict` carrying both the expected and the actual version, which is what
the 412 body renders. The guard is optional at the core level and the HTTP layer is the
consumer that always supplies it — the MCP agent deliberately does not, because requiring a
version would make every update a two-call sequence.

### Delete — the one operation that destroys history

```
DELETE /api/projects/{p}/tasks/{t}
DELETE /api/projects/{p}/notes/{n}
```

**HTTP only. There is no MCP tool for this**, and there should not be: destroying history is
a deliberate human act, and an agent that can delete history is an agent that can cover its
own tracks.

It purges — the entity, and every event carrying its content. What survives is a contentless
tombstone (`note #12`), because a tombstone that quoted the thing it recorded the removal of
would undo the operation. Forgetting a task keeps the notes written while doing it, and their
histories: knowledge outliving its task is why notes are a separate entity at all.

**Why this exists on a project whose first priority is history.** It should not, by those
priorities, and for a long time it did not. Task #15 forced it: `update_note` records the
previous body so a superseded fact stays recoverable, event bodies are FTS-indexed, and there
was no operation anywhere that removed anything. Overwriting a note *preserved* what you were
trying to replace, searchable forever, in a store that is global across every project on the
machine. This is the escape hatch, not a tidiness feature — `archive` remains the normal path.

**What it does not reach.** Forget removes what the store can *attribute* to the entity — its
row, and events carrying its id. It cannot remove text it has no way to link back: a `log`
entry someone typed that quotes the note, or a `note_added` event written before migration 003
(no `note_id` was recorded, and its body is the title with no id to recover it from). Anything
matching by text would be a guess, and a wrong guess deletes another entity's history. So
after forgetting something that genuinely must not persist, search `recall` for it — the
operation is precise, not exhaustive.

```
DELETE /api/projects/{p}      a whole board
DELETE /api/repos/{r}         a repo, off every board and ticket
```

Forgetting a **board** deletes its tasks, notes, workstreams, history and path aliases, so the
next session opened in one of its folders starts a fresh board. Repos it was home to pass to
the earliest other board sharing them; repos only it had go. With no board left to hold a
tombstone, it advances the events sequence instead, which `change_cursor` reads alongside
`MAX(events.id)` — otherwise the delete would be invisible to every live page.

Forgetting a **repo** differs from `DELETE /projects/{p}/repos/{r}` (off one board): it
leaves the store, off every board and every ticket. Its folder's path aliases stay, because
they belong to a board whose history is still there. The tombstone is `repo #3`, on each
board that had it.

The web UI puts both in the repos panel's danger zone, and forgetting a board asks for its
name typed out.

Core: `Store::forget_task` / `forget_note` / `forget_repo` / `forget_board`, `src/core/forget.rs`.

### Errors

```jsonc
{ "error": { "code": "invalid_value", "field": "status",
             "message": "...", "valid": ["backlog","doing","blocked","done","archived"] } }
```

The enum parse failures in `model.rs` already carry the list of valid values — that exists so
errors correct the mistake rather than merely reporting it, and it should survive into HTTP
rather than being flattened to a 400 with a string.

## Live updates

### Why SSE and not WebSocket

The flow is one-directional. The server says "the board changed"; the browser's writes are
ordinary requests that want status codes, error bodies and `If-Match`. Carrying those over a
socket means rebuilding request/response correlation, error semantics and retry by hand, to
gain a direction we do not use.

The deciding argument is resume. `EventSource` reconnects on its own and resends the last id
it saw as `Last-Event-ID` — and this schema already has the monotonic counter that maps onto:
`events.id`. Resume-after-disconnect is therefore free, where WebSocket would need manual
reconnect, manual heartbeat, and a resume protocol invented for the purpose.

### The stream

```
GET /api/stream?project=3
Last-Event-ID: 412

: ping

event: change
id: 415
data: {"project_id":3,"cursor":415,"kinds":["created","status:doing"],"tasks":[12,9]}
```

- `project` is optional. Omitted, the stream carries every project — which is what a
  multi-project overview wants.
- **The payload is a cursor and hints, never rows.** The client refetches what it is
  currently showing. Pushing changed rows means maintaining a second representation of every
  mutation alongside the REST one, for a board small enough to refetch in a single request.
- `: ping` every 20s, so intermediaries do not close an idle stream.
- `event: reset` tells the client to discard and do a full refetch. Sent in two cases: the
  client is too far behind to replay, **and** when `Last-Event-ID` names an id that no longer
  exists — the store was replaced, `AI_KANBAN_DB` was repointed, or an import (#2) renumbered
  it. Resuming an unknown id from zero would replay the entire history; treating it as an
  error would strand the client.

### How the server notices a change

`SELECT MAX(id) FROM events` on a short interval (250–500ms — one indexed row).

The alternatives do not work here. SQLite's `update_hook` only fires for changes made on the
*same connection*, so it is blind to every agent. SQLite has no `LISTEN`/`NOTIFY`. Watching
the `-wal` file with `notify` would cut idle wakeups and is a reasonable later optimization —
it is deliberately not in v1, because it adds a dependency and a platform surface to save
some timer ticks on a single-user local tool.

**One poll for the whole server, not one per connected client.** The poll result fans out to
every stream. This is worth stating because the natural way to write it — a task per
connection — turns ten open tabs into ten times the query load for no benefit.

### The invariant this rests on, and where it is already violated

> **Every mutation must write an event, or it is invisible to the live stream.**

It nearly holds for free: priority #1 already forced every task and note mutation to write an
event, so the history table doubles as the change feed and no separate change-tracking table
is needed. That is a real dividend from the history requirement.

`tests/change_feed.rs::every_mutation_moves_the_change_cursor` enforces it — it walks every
mutation and asserts the cursor advanced. Project creation and path learning both failed it
until #13; a new mutation that forgets an event fails it too, rather than quietly producing a
page that never updates.

**Still open: deletes.** Whatever #11 decides, a delete lowers no `MAX(id)`, so it is
invisible to a cursor that only counts up. Real deletion must write a tombstone event.

### Housekeeping events

`path_learned` (and any kind added to `HOUSEKEEPING_KINDS`) is written for the stream and the
history but filtered out of the agent's `recent`, out of `last_activity`, and out of `recall`.

Alias learning fires the first time a board is used from any new subdirectory. Unfiltered it
would bury real history under "learned path /Users/x/repo/src/core", and make a project an
agent merely walked through report as recently active. The HTTP layer should show these in a
project's detail view — they are the record of how a board came to claim the directories it
claims, which is exactly what someone debugging a split board needs — but never in a feed
that answers "what happened lately".

## Workstreams

Migration 005 added workstreams — named slices of work inside a board (see
`docs/data-model.md`). The human board follows the **same** current workstream the agent's
does. One board, one active workstream, shared by a person and their agents; a UI showing a
different slice than the agent is working in would make the two disagree about what "the
board" is, which is the confusion workstreams exist to remove.

`GET /board` therefore returns two extra fields:

| Field | What it is |
|---|---|
| `workstream` | What the board is scoped to, or `null`. Also what a new task joins by default. |
| `workstreams` | Everything selectable — **including workstreams with no open tasks** |

That second point is a real difference from the agent's board, which hides empty
workstreams because a line reading `contact-form 0` is noise in a response with a token
budget. A person's selector must list them, or a workstream created a moment ago is one
nobody can pick.

### `PUT /api/projects/{p}/workstream`

`{"name": "contact-form"}` scopes the board to an **existing** workstream; `{"name": null}`
widens back out. Names are normalized, so `TYPO3 v13 Upgrade` and `typo3-v13-upgrade` are one
workstream rather than two.

**This API lists workstreams; it does not create them.** An unknown name is a `400` naming
the ones that exist. Creation belongs to the agent, which starts a workstream when it is told
what it is working on — the moment the name is actually known.

The value of refusing is not access control; this is a local, single-user tool. It is the
typo. `normalize_name` folds `Contact Form` into `contact-form`, but nothing folds
`contact-forms`, and a near-miss that silently becomes a third workstream is the same
split-memory failure `project_paths` exists to prevent, one level down. Refusing turns that
into an error that says what exists, which is the self-correcting shape every other invalid
value in this API uses.

**Why the human gets an endpoint when the agent gets no tool.** The agent enters a
workstream as a side effect of asking to see one (`board(workstream: …)`), because for an
agent an extra call is one that gets skipped. A person clicking a control is already stating
intent, and hiding the change behind a side effect would move where their work is filed
without them knowing. Different consumers, different affordances — the same reasoning that
put `origin: user` on every write from this API.

### Filing states where the task lands

`POST /tasks` takes an optional `workstream` name:

- **omitted** — inherit the board's current workstream, exactly as the agent's `task_add` does
- **`""`** — file it unscoped, the explicit escape for general project work while the board
  is scoped to something
- **a name** — join that existing workstream; an unknown one is refused, not created

The new-task dialog always shows the field, prefilled with the current workstream. That is
the point: a task landing in a scope the person could not see was the bug this replaced.

### Moving a task that was filed into the wrong one

`PATCH /tasks/{t}` takes the same `workstream` field, with the same three readings: absent
leaves it alone, `""` moves it out of every workstream, an existing name moves it there. The task
detail response carries `workstream` so the panel can preselect the control with the task's
**own** workstream — not the board's, which would silently re-file every task the panel
touched.

This matters more than it looks. Agents inherit their workstream silently, so a task landing
in the wrong one is the expected error, and before this it was the only field on a task that
could never be corrected.

### Known gap

Task cards do not show which workstream they belong to, so when the board is scoped, work in
that workstream and general unscoped work look alike. `workstream_id` is deliberately absent
from `TASK_COLS` — one of the two things holding `MIN_READABLE_VERSION` at 2, the other being
that unscoped queries never name the column at all (see `docs/data-model.md`; assuming
`TASK_COLS` alone was enough is what shipped a bug). So surfacing it per card needs a separate
lookup, as `GET /tasks/{t}` already does, rather than a wider column list. Worth doing; not
worth reopening that decision for.

## Repos

Migration 007 (see `docs/data-model.md`). A board owns the local checkouts its tickets live
in, and the web UI's **repos** button is the only place they are registered.

- `POST /repos` takes `{"path": "~/work/eee-web", "name": "eee-web"}`. `~/` is expanded here,
  because that is how a person types a path. The name defaults to the folder's, is normalized,
  and is unique across the store. A path that is already a repo — on another board — is
  **shared**: attached here, its home unchanged. A *new* checkout in a directory another board
  already claims — at the path or below it — is a **409 `claimed`** carrying that board's
  `board` and `key`, because the repair is `ai-kanban merge`.
- `PUT /repos/{r}/home` makes this board the repo's home: its folder, and every subdirectory
  the old home had learned, opens here from now on.
- `PATCH /repos/{r}` renames. The path is not editable: a different path is a different
  checkout, and may belong to another board, so moving one is a remove and an add.
- `DELETE /repos/{r}` takes it off this board and this board's tickets. If this was its home
  and other boards have it, the home passes to the earliest of them; from its last board, the
  repo goes and the folder keeps resolving here.
- **No `If-Match` on repo writes.** The guard is for a form held open while an agent writes the
  same row, and no agent writes repos.

On tasks: `POST`/`PATCH /tasks` take `repos` (ids; `[]` clears) and `planio` (`0` clears — a
JSON `null` in an optional field deserializes as "absent", so it cannot mean "clear"). `GET
/tasks/{t}` returns `repos`, with paths, and `planio`. `GET /board` and `GET /tasks` return
`task_links` keyed by task id like `task_tags` — `{"repos": ["eee-web"], "planio": 48213}` —
and `/board` also returns `repos`, every repo with `open` and `total` ticket counts, so the
cards, the pickers and the menu all come from one read.

## Boards by name, moving tickets, All Projects

`POST /api/projects` with `{"name": "BMUKN"}` creates a board for a project that is not one
folder (key `board:bmukn`). A name another board has is refused. Agents reach such a board
through the repos it is home to.

`POST /projects/{p}/tasks/{t}/move` with `{"to": <board id or key>, "log": "..."}` moves a
ticket, `If-Match` required. It is an action rather than a `PATCH` field because it changes the
URL the task lives at. The response carries the task and the board it landed on.

`GET /api/all/board` is the **All Projects** view: every board's tickets in one set of columns,
the same shape as `/board` plus `projects` (so a card can name its board) and
`boards_with_repos` (so "no repo set" follows each card's own board). No workstream — that is a
slice of one board. `GET /api/all/tasks` pages its columns. This is the person's view only: the
agent's cross-board view stays `board(project: "all")`, a per-board summary, because for a
reader paying per token every open task across every project is a pile, not a board.

## Starting work from the board

`POST /projects/{p}/tasks/{t}/start` (body `{"profile": "claude" | "claude-work"}`, default
`claude`) runs Claude Code in the ticket's first repo: in a new tab of the front Ghostty window
when Ghostty is running (through its AppleScript, 1.3+), otherwise — or when that fails, e.g.
AppleScript access was refused — in a new Ghostty window. The response's `opened` says which
(`tab` | `window`). The `claude-work` profile is the same binary with
`CLAUDE_CONFIG_DIR=~/.claude-work`, set through `/usr/bin/env` because neither a tab nor a
window started this way runs the user's shell, so neither sees shell aliases. It
starts with its other repos passed as `--add-dir`, and the ticket as
the first prompt (`render::start_prompt`). The prompt names the board, because the first
repo's home can be a different board. Only a **backlog** ticket can be started — one in doing
already has a session, and one blocked or finished is not ready — and a ticket with no repo is
a `400` too: the session has to start in a checkout.

Each board decides which profiles it allows: `PUT /projects/{p}/profiles/{name}` with
`{"allowed": bool}`, set from the "sessions" section of the board's repos menu. Every profile
is allowed until a board refuses it (migration 009 keeps a deny list), a refused profile is a
`403` from `start`, and the panel does not offer its button. `GET /projects/{p}` and the task
detail carry `profiles: [{name, allowed}]`.

`/api/meta` carries `planio_url` from `AI_KANBAN_PLANIO_URL` (e.g. `https://frs.plan.io`), and
the UI links each Planio number to `<planio_url>/issues/<n>`. A setting rather than a constant:
the tool knows Planio numbers, not whose Planio they are. Unset, numbers show without a link. The launch is logged on the ticket.

It is the one route that starts a process, so it is the one route locked to the board page:
`Host` must be loopback and `Origin`, when sent, must be this server — a cross-site `POST` or a
DNS-rebinding page gets a `403`. The required JSON body also forces a CORS preflight that this
server never answers. No text in a ticket can become a command: a window gets the prompt as one
argv entry, and a tab — which Ghostty only takes as a command line — gets a line made of
variable names alone, with every argument passed as a variable's value. This is a convenience for the person at the board; nothing in
the agent path depends on it, which keeps `serve` on the right side of the no-daemon line.

## Explicitly not in v1

- **Drag-to-reorder.** There is no `position` column and there should not be one.
  `docs/vision.md` names drag-and-drop as the human-board optimization this project is
  explicitly not building for, and `priority` is words-not-numbers for the same reason.
- **Auth, accounts, sharing.** One person and their agents. Localhost is the boundary.
- **Writing history directly** (`POST .../events`), per the non-goal above.

## The non-goal this brushes against

`docs/vision.md` says no daemon. `ai-kanban serve` is a process that stays up.

It stays inside the line on one condition: **nothing else may depend on it.** The agent path —
MCP and both hooks — must keep working with the server never started, as it does today. The
server is a viewer the user launches when they want to look, not infrastructure the system
runs on. The moment something requires `serve` to be running, this project has become the
platform it exists in reaction to.
