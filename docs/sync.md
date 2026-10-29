# Sync rooms: DRAFT PROPOSAL

Status: **proposal, nothing built.** Task #1050 holds the reasoning, including the options
considered and dropped. If accepted, it amends `docs/vision.md` (see
[Vision amendment](#vision-amendment)) before any code lands.

## What this is for

| # | Requirement |
|---|---|
| R1 | A team's agents share work, so nobody solves the same problem twice. "Who is on it" must be fresh **at the moment an agent claims work**. |
| R2 | The store is a valuable asset. Losing a disk must not lose the memory. |
| R3 | Most work is solo, and a board that syncs nowhere must keep working exactly as today. |
| R4 | Existing local boards and their history can join the team. |
| R5 | No setup per repo for the agent. The agent needs no knowledge of sync to use the board (the design law). |
| R6 | Offline keeps working: writes are local first and sync later. |
| R7 | One person on two machines is not a special case. |

## Terms

```
store (one SQLite file per machine)
└── board (a row in `projects`), e.g. "sbb"   identity: git remote, folder or name
    ├── repos          local checkout paths
    ├── workstreams    named slices of work; a task is in at most one
    ├── tasks
    │   └── events     history plus `log` lines (the task's comments)
    ├── notes          durable knowledge, attached to a task or to the board
    └── board events   decisions and summaries not tied to a task
```

- **Room:** the shared copy of one board's content on a server. A board is **connected** to at
  most one room. Connected means **all** of the board is shared.
- **Workstreams are not affected.** They stay a grouping inside the board and sync by name.
  Considered and dropped: one protected "remote" workstream as the entrance to the room. A task
  holds one workstream, so shared tasks could not also be in "deploy" or "db-migration".

## The idea in one paragraph

Every machine keeps its full local store, exactly as today. A **room** lives on a self-hosted
server and holds a shared set of tasks with their history and notes. A local board can be
**connected** to one room. From then on, everything written on that board is published to
the room, and everything others publish into the room arrives on the board. The board stays
local: its name, folders, repos and current workstream are each person's own. Benny's
"sbahn v13" and Volker's "sbahn upgrade" can be the same room.

Why this shape:

- **Local first.** Hooks keep reading a local file and never touch the network. Offline is full
  speed. Every member holds a full copy of the room, which covers R2 better than any single server.
- **The room is the unit, not the store.** Private boards never leave the machine, so there is no
  access-control system to build.
- **The server relays.** It authenticates, orders changes and serves "changes since X". It does
  not run board logic, so it seldom needs upgrading in step with clients.

Considered and dropped (reasons on #1050): a central server as the only copy (no offline
work, and the hooks would need the network), literal git files in the repo (tasks would follow
branch switches; a board that spans several repos has no single repo), libSQL/Turso (a vendor
rewrite and a single writer), cr-sqlite (last push 2024-10, native extension). The cr-sqlite
*approach* is borrowed: for each field, the newest write wins.

## Rules

| Rule | Why |
|---|---|
| A local board connects to **at most one room**, and on each machine a room connects to **at most one local board**. | Several rooms per board would make every `task_add` choose a room, which breaks the design law. Two local boards in one room would put two copies of the same room on one machine. |
| **Everything on a connected board publishes.** No per-task privacy. | Per-task privacy means the agent decides what is private. Private work goes on a board that isn't connected. |
| A room carries **tasks, their events, notes (including board-level notes) and board-level events**. | Notes and decisions are the knowledge half. A teammate's `recall` finding your root cause is the double work avoided most often. |
| Sync is **both ways for every task, your own included.** | Teammates log evidence and findings on your tasks, and you need that back. Publishing without receiving would leave your copy behind the room's. |
| Synced per room: workstream **membership**, by name. Local only: `current_workstream`, `project_paths`, `repos.path`. | Which workstream a task belongs to is shared. Which one *you* are in is your context. This also prevents one person's workstream switch from changing everyone's view. |
| **Connecting is a CLI step,** never the agent's: `ai-kanban room join <server>/<room>`. | Joining publishes the board's whole history to others, and that is a human decision. |
| A first connect publishes the board's existing tasks, history and notes. When two existing boards join one room, the result is the **union**. | That is what R4 asks for. Duplicates get reconciled by people, by forgetting or merging one, since only people know two tasks are the same. |
| `task_update(move_to)` **onto** a connected board publishes the task. **Off** one, the task leaves the room: others keep a frozen, archived copy with "moved off the team board by benny". | The agent can move tasks, so both directions need a defined result. Deleting the task on others' machines would destroy their history of it. |

## Connecting a board

**One remote per client** in v1: a store has one origin tag, and no command ever has to say
which server it means.

**The usual way in is the plugin's settings.** Claude Code asks for them when the plugin is
enabled, or under `/plugin` → ai-kanban → Configure: *Team relay URL* and *Team relay key*, both
optional (`userConfig` in `plugin/.claude-plugin/plugin.json`). The key is `sensitive`, so it is
kept in the system keychain and never typed into a conversation. The MCP server gets both
through its environment and, at startup, saves them as this machine's remote (`remote.json`,
below) **if it has none** -- locally, without asking the relay anything, so an offline start is
not slowed. The first sync round then asks the relay whose key it is, and the store takes that
tag (a store with no tag has never published, so it has nothing to collide with; a key copied
from another machine is still caught by the high-water check, see "When sync pauses").

An existing remote always wins: settings naming another relay or key are not taken, and every
`board` answer opens with a heads-up that says so and how to switch (`ai-kanban remote
--disconnect`, then a new session). Overwriting would move the store to another identity, which
`remote` itself refuses while boards are connected. This also settles two Claude configurations
on one machine (`~/.claude`, `~/.claude-private`): there is one store and one remote, and
whichever was set first stays. Decided 2026-10-29, after the first marketplace install showed
that without it the key had to go through a terminal or the chat.

The commands below do the same by hand, and the rest of connecting. Claude can run them when
asked (the plugin puts `ai-kanban` on its shell's PATH); none of them needs the key once the
remote is set.

```
ai-kanban remote https://sync.board.rannow.de --key akb_…   set the remote, check the key
ai-kanban remote                                            show it: URL, user, origin tag, protocol
ai-kanban room list                                         rooms on the remote, and which local board each is connected to
ai-kanban room connect sbahn [--board sbb] [--yes]          connect a local board to a room
ai-kanban room disconnect sbahn [--keep-foreign|--wipe-foreign] [--yes]
ai-kanban remote --disconnect [...same flags]               disconnect every room, then forget the key
```

**`remote <url> --key`** checks the key at once (`/v1/hello` and `/v1/whoami`), so a typo fails
here and not mid-session. The key is stored in `remote.json` next to the store (mode 0600),
never in SQLite, so `backup` and `export` never carry it. `AI_KANBAN_SERVER_URL` and
`AI_KANBAN_SERVER_KEY` cover CI. Setting a different URL while rooms are connected is refused:
disconnect first. Moving the domain is `remote --move <new-url>`, the same server under a new name.

**`room connect sbahn`:**

```
$ ai-kanban room connect sbahn
Connect which local board to room "sbahn"?
  1) sbb            (this folder)
  2) ai-kanban
  3) eee-web
Board [1]:
board "sbb" -> room "sbahn"
  will publish:  42 tasks, 130 events, 18 notes   (as #N-b1)
  will receive:  11 tasks from volker (v1)
Connect? [y/N]: y
backup written: ~/…/kanban-before-connect-sbahn.db
connected. Board header now shows: Board: sbb @sbahn
```

- **The picker is a numbered list read from stdin**, so no crate is needed. The board found from the
  current folder is preselected. Boards already in a room are not listed.
- A board connects to at most one room, and a room to at most one board on this client. Both are
  enforced, and the refusal names the existing link.
- The publish summary and the confirmation are mandatory, because connecting publishes the board's
  whole history. A backup is written before anything leaves the machine.
- **Connecting to a room that does not exist creates it.** Any user may, since a server is a team.
  The summary says `room "sbahnn" does not exist yet — it will be created`, so a typo is visible
  before the confirmation. Only the admin deletes rooms (`ai-kanban-relay room delete`). The
  server removes **empty** rooms after 7 days: no tasks, notes or events, and no board connected.

**`room disconnect sbahn`:**

```
$ ai-kanban room disconnect sbahn
Board "sbb" and room "sbahn" will be uncoupled.
  3 local changes are not pushed yet.  [s]ync now / [d]iscard / [c]ancel: s
  synced.
  11 tasks on this board came from other members (v1).
  Wipe them locally? They stay in the room.  [k]eep / [w]ipe [k]: k
disconnected. Board "sbb" is local again.
```

- **Unsent changes come first.** Disconnecting silently would withhold them from the team.
- **Keep is the default for foreign tasks.** Wiping saves only space, and the server still has them.
  Kept ones stay readable with their `#N-v1` handles and stop changing.
- **Reconnecting resumes cleanly:** identity is (origin, id), so kept tasks are matched again rather
  than duplicated.
- Nothing is deleted on the server, and your own published tasks stay in the room for the others.

**`remote --disconnect`** runs the disconnect for every connected room, with the same questions
for each, then removes the key. The first prompt says so: "All connected boards will be uncoupled
from <url> and the key removed."

**Without a terminal** (scripts, `tests/cli.rs`), every prompt needs its flag. Missing flags fail with
the list of them, and nothing is half-done.

**Later: a repo names its room.** A checked-in `.ai-kanban.json` with `{ "room": "sbahn" }`. If the
board is new and empty and the remote is set, it connects automatically, because nothing local is
published. If the board has content, the header shows a hint instead. It only ever names a room on
the remote this client already trusts.

## Identity: what the agent sees

The deciding constraint: people and agents pass task references to each other ("I'm doing
#122, #423 and #433 today"), write them into logs, note bodies and commits, and core generates
`#N` prose itself (`describe_change`, `move_task`). A reference to a shared task must
therefore be **the same on every machine, valid the moment the task is created (offline
included), and recognisable by the person who created it.**

**The handle is the creator's own id plus the creator's origin tag: `#122-v1`.**

Two different things are called "id" here. The **stored id** stays an INTEGER rowid, per machine,
exactly as today. The **handle** is a string, made from that integer and the origin tag, and it is
what agents and people see and pass to tools. Only the handle crosses machines.

- **The origin tag** belongs to one store, which in practice means one machine. It is chosen
  when that machine's key is created (`v1` = Volker's laptop, `b1` and `b2` = Benny's laptop and
  desktop) and is unique on the server. It survives key rotation. The board header of a
  connected board lists who each tag is: `team: b1 benny, v1 volker`.
- **The number is the creator's local id, unchanged.** Volker's "#122" is the same number on
  your machine. Nothing is leased from the server and nothing is assigned at sync time.
  Connecting a board renames nothing: your `#775` becomes `#775-b1`, and the number in old logs
  still matches.
- **On a connected board, every task shows its handle, on the creator's machine too.** Volker
  sees `#122-v1`, not `#122`. Otherwise his "#122" means your local #122 on your machine. So a
  plain `#N` always means a task created on this machine.
- **The `task` param becomes a string** in the tool schema: `122-v1`, `#122-v1`, or `122` meaning
  "created on this machine". A JSON integer is still accepted, because agents that learned the old
  schema will send one. The precedent for accepting either form is `834cf23`.
- **Core-generated prose** ("blocked by #122-v1", "moved from …") uses the handle on a
  connected board. Agents copy what they see, so their own text follows.
- **Notes keep their local ids.** A note id is only ever passed back to `note_update` on the
  same machine, and nobody tells a teammate a note number, so a second handle namespace would
  cost tokens for nothing.
- **Unknown handles and mixed-up numbers fail loudly.** A plain number that is the hidden local
  id of a teammate's task is refused with that task's handle, because the number is never shown
  and the likeliest way to arrive at it is someone's "#40" meaning *their* #40.
- **(origin tag, origin id) is also the sync identity.** No hidden random id is needed.
- **Arriving tasks get a local rowid,** as today, so the insert paths and the live feed's
  `MAX(events.id)` cursor are unchanged. That rowid is internal and **never rendered** for a task
  from another origin.

Collisions cannot happen:

| | Benny's store (b1) | Volker's store (v1) | Everyone sees |
|---|---|---|---|
| Benny files a task | local 812 | arrives as local 40 (hidden) | `#812-b1` |
| Volker files a task | arrives as local 813 (hidden) | local 122 | `#122-v1` |
| Benny, on a board that isn't connected | local 814 | does not exist | `#814` (Benny only) |

**Requires that ids are never reused.** Tasks and notes are a plain `INTEGER PRIMARY KEY`, and
`forget` deletes the row (`forget.rs:54`, `:87`). So forgetting the newest task hands its id to
the next one. Under sync, that issues `#122-v1` twice, and the second task collides with the
first one's forget tombstone on every machine. Migration 004 fixed exactly this for events.
Phase 1 does the same for tasks and notes: either rebuild them as AUTOINCREMENT, or keep a
high-water mark that the two insert paths respect. The rebuild is heavier, because of the FTS
triggers and the cascades that 010 avoided.

**Text from another origin is qualified for display.** A plain `#N` inside text that came from
another origin (an event body, a note, a task body) is rendered as `#N-<origin>`. The stored text is
never rewritten, and the origin of every synced row is already known from its identity. This
happens **always**, even if no such task is known in the room. Misreading Volker's `#775` as your own
#775 is the worse error. A GitHub issue `#12` showing as `#12-b1` is cosmetic. This covers text
written before the board connected, and agents that write a plain number.

## People, not sessions

- The server derives the **author** from the key. A client never sends it. One **user**, many keys:
  **one key per machine**, each under its own origin tag, which the relay picks (`v1`, then `v2`).
  R7 needs nothing more. Both machines' boards join the same room, and both appear as "volker".
  One key per machine is not bookkeeping: a store's task numbers are its own, so two machines
  under one tag would both publish `#1-v1`. A copied key is caught (see "Pauses") before it
  publishes anything. Keys carry no label: the tag already tells machines apart, and a label
  solved nothing anyone had (decided 2026-10-28).
- Agent sessions stay local. `events.actor` (`user | agent | system`) is unchanged.
- "Who is on it" = everyone who moved it into `doing` since it last left `doing` (the origin tags
  of those `status:doing` events, earliest first), or the creator of a task filed straight into
  `doing`. **No assignee column.** On a shared board a `doing` row says `by volker` (or `by you`,
  or `by volker, you` when two are on it), and the team line counts it:
  `team: a1 benny (you, 2 doing), b1 volker (1 doing)`. Measured in `tests/budget.rs`.
- **Claiming what someone else has in `doing`** still goes through (a warning, not a lock: two
  people on one task can be deliberate), but the answer leads with `Heads-up: volker has had
  #12-v1 in doing since 20m ago. You are both on it now -- check with volker before starting.`
  The catch-up round just before it makes that current, and it is skipped when it would add
  nothing: while the room's notification stream is live (it spoke within 40s; the relay sends a
  heartbeat every 30s) and everything it announced is already pulled, the local copy is as
  current as a round would make it. Measured from far away (280ms to the relay), that took
  `task_add` and claims from 0.6s to 0.01s. Without a live stream, or with an announced change
  not yet pulled, the bounded round runs as before. What neither catches is a teammate's claim
  from the last second or so, before their session has pushed it; #1068 then shows both names.
- **Joining is recorded as a claim.** The status is already `doing`, so no field changes, but a
  `status:doing` event is written anyway and syncs. Otherwise only the one joining would know:
  the person already working on it, who most needs to hear about it, would see nothing (#1068,
  found in a live test). Claims made in the same moment, before either side saw the other's,
  both stay listed for the same reason: "the latest claim" would silently drop one. Filing
  straight into `doing` on a shared board writes that event too, so the filer stays listed when
  someone joins. Only the agent's explicit `doing` claims: the web form sends `status` with every
  save, and editing a body is not taking the task on.
- Known edge: "since it last left `doing`" orders events by their whole-second timestamp, so a
  `blocked` and a `doing` from two people in the same second can be ordered differently on two
  machines, and the claimer lists differ until the next status change. Rare; accepted.
- **No "or move it back"** in the heads-up. The status is shared, so moving it back would take
  the task from the teammate too. There is no way to withdraw a joined claim short of a status
  change; accepted until someone needs one.
- **Filing on a shared board** checks the open tasks for one whose title shares most of the new
  one's significant words (and at least two): `Possible duplicate of what you just filed: #7-v1
  Login times out behind the proxy (doing, by volker)`. Words rather than FTS rank, because bm25
  has no threshold that means "the same task", and a hint that fires on every filing gets ignored.
  Shared boards only, so private boards render as they always did.

## How sync works

### Changes

Each mutation on a connected board writes, **in the same transaction**, one change per
**changed field** (never a whole row) into a local outbox:

```
{ cid:     "b1:5012",                               change id: origin tag + outbox seq
  entity:  { kind: "task", origin: "v1", id: 122 },  what changed (kind: task | note | event)
  subject: { kind: "task", origin: "v1", id: 122 },  what a forget of it removes, if anything
  field:   "status",                                 absent for insert-only entities (events)
  value:   "doing",                                  never read by the server
  clock:   { wall_ms, counter, tag: "b1" } }
```

- **`cid`** makes a retried push harmless: the server stores each `cid` once. The outbox seq is
  AUTOINCREMENT, because a drained outbox would otherwise reuse seqs (the bug #1064 describes for
  tasks).
- **`subject`** mirrors what a local forget deletes (`src/core/forget.rs`). Task fields, task links
  and a task's own events have the task as subject. Note fields, note paths and **any event about a
  note** have the note as subject, because forgetting a task detaches its notes and their history
  rather than deleting them. Board-level events have no subject. The kind is part of it because
  task and note ids are separate sequences.
- Task repos, tags and note paths are **fields holding the whole list** (`repos`, `tags`, `paths`),
  so the newest write wins per list. That avoids adding set semantics to the protocol.

- **Clock:** a hybrid logical clock (wall time, then a counter, then the origin tag as the
  tie-break). Not `updated_at`, which is whole seconds, so ties are routine (note #3).
- **The clock is taken at the edit, not at the sync.** The dirty triggers stamp each edited row
  (`sync_edited`, migration 017), and flush publishes with that stamp. Stamping at flush time
  made the side that synced *last* win: a status set offline in the morning and synced in the
  evening reopened a task a teammate had closed at noon (#1069, found in a live race test). The
  stamp is never behind anything this store has received, so an edit made after seeing a
  teammate's change still beats it when their clock runs ahead. Granularity is the row: two
  fields of one task edited hours apart offline both carry the later time.
- **Apply:** for each field, apply if the incoming clock beats the stored clock for that field,
  else ignore. Order-independent and idempotent, so a change applied twice or late lands the
  same way. Tested as a property: any order of the same change set gives the same state.
- **Events** are insert-only: one change carries the whole row, and duplicates are dropped by id.
- **Forget** is a tombstone that beats every later field change and is **kept forever**.
  Otherwise a member who was offline pushes the forgotten rows back. **In a room, only the admin
  forgets** (`ai-kanban-relay forget <room> <handle>`), because it is permanent for every member.
  Locally, forget on a connected board is refused and the message names that command. On boards
  that aren't connected, forget works as today.
- **Names that must be unique** (workstreams per board, repos) merge **by name**. Two machines
  creating `deploy` offline get one workstream, not a constraint failure.
- **Repos travel as keys:** the normalized git remote (the `git:…` form of `projects.key`), or
  `name:<name>` for a checkout without one. Each member's task links to their own checkout of
  that remote, whatever it is called there. A key with no checkout on this machine is kept as
  *unmapped* and shown ("no checkout on this machine"), so a local edit never drops a teammate's
  link and the task does not read as "no repo set". Adding the checkout later adopts those links.
- **Which checkouts a machine has is local setup.** Taking a repo off your board, or forgetting it,
  keeps its links on shared tasks as unmapped instead of removing the repo from the team's tasks.

### How a write becomes a change, and which events travel

- **Triggers, not calls in each write path.** Migration 014's triggers mark every inserted or
  updated row on a connected board as dirty. `flush` compares each dirty row's synced fields
  with the last value published to or received from the room (`sync_fields`), and writes a
  change only for fields that differ. Forget, merge, import, the HTTP API and future write paths
  are covered without having to remember to be. Every column is classified as synced or local
  in `src/core/sync.rs`, and a test fails on a column in neither list.
- **Received changes are written straight into the rows**, never through `create_task` or
  `update_task`, which would write duplicate events. Each received change is recorded in
  `sync_fields` too, so the next flush has nothing to echo back. Pending local writes are
  flushed before applying, in the same transaction, so a received change can never overwrite
  an unpublished local edit.
- **Events travel by allow-list:** `created`, `updated`, `status:*`, `note_added`,
  `note_updated`, `log`. Everything else stays on the machine, including kinds added later.
  Several other kinds describe one machine's setup: `repo_added` carries a checkout path, and
  `project_created`, `moved` and `moved_out` name local boards ("moved here from board
  customer-x").
- **Disconnecting drops unsent changes and unrecords them**, so reconnecting publishes those
  edits after all instead of finding "nothing changed".

### When it happens

All of it runs in a background thread of the MCP process (`src/remote/daemon.rs`), which exists
per session already, so there is no daemon of its own. Nothing starts unless a remote is set
and the run allows sync.

- **At startup**, in the background: never in the way of the session starting.
- **Shortly after every write** the agent makes: writes in quick succession go out as one round.
- **When a teammate pushes.** One SSE stream per connected room (`rooms/{r}/events`) carries only
  "room changed, head N", plus a heartbeat every 30s. Each notification triggers a round that pulls
  from the board's own cursor. SSE rather than WebSocket, because only server -> client is needed,
  it is plain HTTP, and the codebase already serves SSE. Behind nginx it needs `proxy_buffering
  off` and a `proxy_read_timeout` above 30s. Streams reconnect with backoff.
- **Every 60s regardless.** Streams drop, laptops sleep, proxies cut idle connections, and a missed
  notification may only delay a pull, never lose one. While the relay is unreachable or sync is
  paused, rounds back off up to every five minutes.
- **Before a claim** (`task_update` to `doing`) **and before `task_add`**: a round on the tool
  call's own connection, so what the agent sees is current. No request starts after 2.5s and each
  is bounded at 1.2s, so even a relay that accepts connections and never answers costs the agent
  about that much. It skips refreshing the member list (the background round does that). When the
  relay cannot be reached the claim still goes through, and the header says so.
- **The syncer starts with the session.** A session that was already running when the remote was
  set or a board connected does not sync until it is restarted; `ai-kanban room sync` covers the
  gap by hand.
- **Several sessions on one store sync independently.** There is no lock held across the network:
  retries are stored once per change id, an older clock never wins, and the cursor only moves
  forward, so concurrent rounds agree (`tests/remote_sync.rs`).
- **`AI_KANBAN_SYNC=off`** keeps one run local-only (set in the shell that starts Claude Code; not
  yet verified to reach plugin MCP servers from `.claude/settings.json`). Connected boards keep
  recording, and the next run that allows sync publishes the backlog.
- Applied remote changes arrive with their events, so `tests/change_feed.rs` and the live page see
  them like any other write.

### What the board header says

A shared board's header names its room, and adds one line only when there is something to say:

```
Board: sbb @sbahn  (5 open, 2 doing)
team: b1 benny, v1 volker  (offline, 3 tasks, 1 note unsent)
```

Unsent work is counted by what it is about, not by change: one new task is a dozen field changes,
and the first live test's `39 changes unsent` for three tasks read as something gone wrong.
`room status` and the disconnect prompt keep the exact change count.

The second line can also say `not synced yet`, `last sync 3h ago` (a stale local copy must say
so), `sync off for this run`, `N parked until ai-kanban is upgraded`, or replace everything with
`sync paused: <why and what to do>`. Only an unreachable relay reads as `offline`, which passes by
itself; any other failure reads as `sync failing: key rejected` / `room not found` / ..., because
someone has to act. A board that is not shared renders exactly as before.

### When sync pauses

Sync stops for the whole store, never local work, when continuing would corrupt the room:

- the relay speaks a protocol this build does not (upgrade one side);
- the key belongs to a different origin tag than the store publishes under;
- **the relay already holds higher ids from this tag than the store would hand out next** -- a
  store restored from a backup, or copied. Its next tasks would reuse numbers the room holds.
  A second machine using a copied key looks exactly the same, and is caught the same way.
  The way out: `ai-kanban room disconnect <room> --keep-foreign`, a key of its own from the
  relay admin (`key add <user>`), `ai-kanban remote <url> --key <new key>`, then
  `ai-kanban room connect` again. Work
  published under the old tag stays the store's own (`sync_own_tags`).

A good check lifts the pause by itself; `ai-kanban remote` and `ai-kanban room status` show it.

### Hooks

Unchanged. They read the local file, read-only, and never touch the network. The cost: the
SessionStart view is as fresh as the last sync, which may be the previous session. Freshness
at claim time comes from the pull before `doing`, not from the hook.

## Versioning

Yes, and separately from the schema version. `transfer.rs` already sets the precedent:
`FORMAT_VERSION` is deliberately not the schema version, because the two change for
different reasons.

- **Protocol version** in every request. `GET /v1/hello` returns the range the server supports.
  On a mismatch **sync pauses and local work continues**: `@sbahn (sync paused: the server
  needs ai-kanban ≥ 0.9 — run make install)`. Local-first means a version problem never blocks
  work.
- **Changes are per field, so unknown fields are harmless.** An older client receiving a field
  it does not have **parks** the change instead of dropping it.
- **Unknown values are the real trap.** Migration 010 added `testing` by rewriting a CHECK
  constraint, so a client one version behind cannot apply `status = testing`. Rule: a change
  that fails to apply is **parked** (kept in a local table, counted in the header), and re-applied
  after an upgrade. Late application is correct because the field clock decides, not arrival order.
- **The server does not validate values.** It checks the envelope (room, id, clock, protocol)
  and auth, and stores the rest unread. So a new status or field needs only client upgrades, and
  members upgrade at their own pace.
- Breaking the envelope means `/v2`, served alongside `/v1` for a transition.

## Server

`ai-kanban-relay`, its own binary (`crates/relay`), as a Docker image with a `/data` volume. TLS goes on the reverse
proxy. It has its own router with bearer auth on every route; `serve`'s loopback-only rules
stay for `serve`.

```
ai-kanban-relay user add volker                       # the user, tag v1 and a key, printed once
ai-kanban-relay key add volker                        # a second machine: tag v2, its own key
ai-kanban-relay key list | revoke <id> | rotate <user> [--tag v2]
ai-kanban-relay room list | delete <room>               # rooms are created by connecting
ai-kanban-relay forget <room> <handle>                 # the only forget in a room
ai-kanban-relay snapshot                              # also scheduled: --snapshot-every 6h --keep 28
```

- **Keys:** 32 random bytes, prefixed `akb_`, stored as SHA-256. Not JWT: one server with a
  database gains nothing from stateless tokens, and revoking a key is a row delete.
- **v1:** every user on a server may join every room. A team is a server. Need separation? Run
  a second container.
- **Routes:** `hello`, `whoami`, `rooms`, `rooms/{r}/join` (creates if missing), `rooms/{r}/changes`
  (POST to push, GET `?since=` to pull), `rooms/{r}/events` (SSE notifications).
- **Housekeeping:** empty rooms are removed after 7 days.
- **Storage: its own SQLite file and schema** (`--db`, default `/data/server.db`), never the
  kanban store. It holds users, keys, rooms, members and the change log, in order. The server only
  relays, so tying it to core's frozen schema would couple it to every core migration. It never
  falls back to `AI_KANBAN_DB`, so a server started on a laptop cannot open the personal store, and
  it refuses to open a file that isn't a server database. The log is what a new member replays.
  Snapshots of it cover mistakes, and off-site backup is the operator's job. No board is
  materialized on the server in v1.
- **What the relay still validates without reading values:** the author comes from the key; the
  clock's `tag` must equal the key's tag, or a client could win conflicts under someone else's name;
  `cid` must start with the key's tag. Tags match `^[a-z][a-z0-9]{0,7}$` (no `-`, because handles
  are parsed from strings), and `srv` is reserved for the server's own changes. Request bodies and
  batch sizes are capped.
- **Forget in a room** (`server forget <room> task 122-v1`) deletes every change whose entity or
  subject is that task, appends a content-free tombstone authored by `srv`, and remembers the
  subject, so a member who was offline cannot push it back.
- **Restoring a client from backup rewinds its counters.** A restored store would reissue task ids
  (`#812-b1` twice in a room) and outbox seqs (new changes dropped as "retries"). So the server
  tracks, per tag, the highest outbox seq and the highest origin id per kind it has accepted.
  `whoami` returns them, and a client whose local counters are lower **stops syncing** and asks
  for a key of its own (a new tag). As a second net, a `cid` that arrives again with different content is refused.
- **Bind:** `--listen` defaults to `127.0.0.1:7373`. The container passes `0.0.0.0:7373`. `hello`
  is the only route without a key.
- **SSE notifications come from polling** `MAX(seq)` per room, not from the push handler, for the
  same reason `src/http/stream.rs` polls: admin commands run in another process (`docker exec`),
  and an admin forget must notify too.

### Running the relay

The image is built by `Dockerfile` and published by `.github/workflows/relay-image.yml` to
`ghcr.io/<owner>/ai-kanban-relay`, for amd64 and arm64. Tags: `:edge` from main, `:1.2.3` and
`:latest` from a `v1.2.3` tag.

```
docker run -d --name kanban --restart unless-stopped \
  -v kanban-data:/data -p 127.0.0.1:7373:7373 ghcr.io/<owner>/ai-kanban-relay:latest
docker exec kanban ai-kanban-relay user add benny   # prints benny's key once (tag b1)
```

- **`deploy/relay/`** holds the compose file of the real deployment and `kanban`, a helper that
  lives next to it: `./kanban` shows status, users and rooms; `./kanban invite <name>` creates
  the user and prints a message to send them (plugin install, URL, key); the admin commands pass
  through (`./kanban key rotate volker`). `./kanban help`, `./kanban help key` or `./kanban key
  --help` show what each does, with examples. Those pages and the messages come from the relay
  binary (`crates/relay/src/pages.rs`), so they match the running image; the helper passes the
  name it was called by and the public URL (`AI_KANBAN_RELAY_AS`, `AI_KANBAN_RELAY_URL`). It wraps `docker compose exec`, because the image has
  no shell or sqlite3 to administer it with otherwise.
- **`--debug`** (or `AI_KANBAN_RELAY_DEBUG=1`; on the server `./kanban debug on|off`) logs every
  request, response and event-stream message, one line each: `HH:MM:SS.mmm #<n> > GET /v1/... user/tag
  <bytes> <body>`, then `#<n> < 200 3ms ...`, and for a stream `sse open`, each `sse head N`, `sse
  closed after Ns: <why>`. It is a middleware, so a request turned away before any handler (no key,
  a body that is not JSON) shows too. The key never appears: a request shows the user/tag it
  authenticated as, or the key's last four characters. Bodies do, so it is off unless asked for.
  The 30s keep-alives come from axum, not the stream, and do not show. `crates/relay/src/debug.rs`.
- **Test clients on one machine:** `scripts/kanban-test` runs further clients next to your own
  plugin session, each with its own store, Claude Code config and plugin copy on this checkout's
  build (`kanban-test claude b2`), plus a local relay with `--debug` (`kanban-test relay`,
  `kanban-test key <user>`). Its own config dir, not just another `AI_KANBAN_DB`: under your normal
  config the installed plugin loads too, its relay key is adopted by the fresh test store (one key,
  two stores, sync pauses), and the `install-cli` link is re-pointed at the test build. For the
  same reason a test client ignores the plugin's relay settings: its relay comes only from
  `kanban-test cli <name> remote <url> --key K`, with a key of its own (`./kanban invite <user>
  --machine`). On a freshly wiped relay the high-water check would not catch a shared key, since
  both stores start at zero.
- **Runs as non-root** (distroless, uid 65532) and owns `/data`. A bind mount instead of a named
  volume must be writable by that uid, or the container runs as the folder's owner instead
  (compose: `user: "1000:1000"`). Otherwise it stops at once with `unable to open database file`.
  Found on the first real deployment. Docker Desktop on macOS maps ownership differently and does
  not reproduce it, so test this on the Linux host.
- **Snapshots** every 6h into `/data/snapshots`, the newest 28 kept. That is the same volume, so
  copy the volume off the host as well.
- **TLS goes on the proxy.** Publish the port on loopback only, as above, and let nginx (or
  similar) terminate TLS. The event stream needs `proxy_buffering off;` and a `proxy_read_timeout`
  above the 30s heartbeat.
- `HEALTHCHECK` runs `ai-kanban-relay health`, a single request made with the standard library,
  because the image has no shell or curl.

## Local schema

All sync state lives in **new side tables**. `tasks`, `notes` and `events` keep their shape, so
`TASK_COLS` and `MIN_READABLE_VERSION` are untouched (notes #13, #17, #71):

| Table | Holds |
|---|---|
| `sync_rooms` | board ↔ room, pull cursor |
| `sync_ids` | (entity, local id) ↔ (origin tag, origin id) |
| `sync_clocks` | (sync id, field) → clock |
| `sync_outbox` | changes not yet pushed |
| `sync_parked` | received changes that could not be applied, and why |
| `sync_node` | this store's origin tag, as issued with its key |

The read-only hook reads `sync_rooms` and `sync_ids` (for the header and the handles) with
tolerant queries, so an unmigrated store shows the board without them.

## Costs that remain

- **Every write path on a connected board records field changes.** It is the same discipline as
  "every mutation writes an event", enforced the same way: `tests/change_feed.rs` grows an
  assertion that every mutation also writes its changes.
- **Rendering handles** touches every place a task reference is printed, prose included.
- **Concurrent edits of one field:** the newest clock wins. Nothing is lost (history and the
  previous note body keep the other version), but the visible value can be the loser's.
  **Accepted** (2026-10-28): the last write wins, and the history shows both.
- **Two people claim the same task within one sync window:** both succeed, and the next sync
  shows two names. The claim warning makes this rare.

## Phases

0. **Now, independent of all of this:** a scheduled `ai-kanban backup` copied off the machine.
1. **Core, no network:** node id and clock, side tables, recording field changes, the apply
   function and its order-independence test, ids that are never reused (tasks, notes),
   handles in rendering and in tool params.
2. **Server:** relay, users and keys with origin tags, rooms, `hello`, snapshots, Docker image.
3. **Client:** `remote`, `room list|connect|disconnect`, push/pull and the SSE stream in the MCP
   process, lock, parking, header line, pull before claim.
4. **Coordination** (built): who has each `doing` task, the claim warning, the duplicate hint.

## Decided (2026-10-28)

- A room carries notes and board-level events as well as tasks.
- In a room, only the admin forgets.
- Any user creates a room by connecting to it. The admin deletes rooms, and empty rooms go after 7 days.
- Concurrent edits: the last write wins, and the history shows both.
- The `task` param becomes a string (an integer is still accepted).
- New dependencies approved: `ureq` (HTTP client with rustls, also reads the SSE stream),
  `getrandom` (keys), `sha2` (hashing keys). Each needs a row in `architecture.md`.

## Open decisions

- **The vision amendment** below, which lands before any code.

## Later, not v1

- **A per-task "private" flag on a connected board.** Only a human may set it (terminal or UI),
  or the agent ends up deciding what is private. Whatever points at a private task stays local
  too: `blocked_by`, and notes attached to it.
- **Several rooms per board.**

## Vision amendment

- "Not a team planning tool. One person and their agents." → "One person and their agents,
  optionally sharing a board with a team through a self-hosted room. Still no assignees,
  sprints or workflow ceremony."
- "no server, no daemon" → "no server **required** and no daemon. A team may run one relay."
