# ai-kanban — foundational docs

> **⚠️ This is a founding decision record, not a live specification.**
>
> Written in the project's first session, before any code or documentation existed. It describes
> docs that did not yet exist at the time of writing.
>
> **How to treat this file:**
> - **The `docs/` files are authoritative.** Where this plan and a real doc disagree, the doc wins —
>   this file is not updated when they change.
> - **Do not implement from this directly.** Check `docs/data-model.md` and `docs/tool-design.md`
>   for the current schema and tool surface. Anything here may have been revised or dropped.
> - **What stays durable is the *why*.** The decision table and the footgun audit record the
>   reasoning behind SQLite, MCP, auto-resolved projects, the three-entity memory model, and Rust —
>   including options that were considered and rejected. That context is hard to reconstruct and is
>   the reason this file is kept.
> - **The adoption mechanism was deliberately left open** here. If it has since been decided, that
>   decision lives in `docs/adoption.md`, not in this file.


## Context

`ai-kanban` is a greenfield project (empty repo, no commits, only `reference/` with Claude Code docs
and an external `ToolDesign.md` borrowed for its mentality). This session produces **documentation
only** — no code, no schema file, no server.

**What it is:** a kanban board whose first-class consumer is an AI agent, not a human. The human can
observe and update, but the agent is the primary user.

**Priorities, in the order they matter:**

1. **Persistent memory and project history** — an agent starting cold knows what's in flight, what
   happened before, and what was learned about the codebase.
2. **Agent self-organization** — an agent that spots a side quest, a bug, or a TODO files a task
   itself, without being told.
3. **Multi-agent orchestration** — minor to purely optional. Not designed for.

**The problem this exists to solve:** agents are natural shortcutters. Knowledge dies at the end of a
session, and the next session rediscovers it. Previous attempts at this problem (the user's words)
became "entire platforms with integrated multi-orchestrated shells per task" — whole ecosystems. This
is deliberately not that: a simple tool with little setup overhead that is "just" a knowledge system
over many sessions.

**Why docs first:** the storage model and tool surface are cheap to argue with on paper and expensive
to change in code. The docs must pin both down precisely enough that building is mechanical.

---

## Decisions already made (do not relitigate)

These were settled in conversation. They are recorded here with their reasoning so the docs can
carry the *why*, not just the *what*.

| Decision | Reasoning |
|---|---|
| **SQLite**, single global DB outside any repo | A knowledge system over many sessions needs real search (FTS5) and real queries; grep over hundreds of files degrades, SQL doesn't. Global means no binary blob in git, no merge conflicts, and **zero per-project setup**. |
| **MCP** as the agent interface | Rejected files-in-repo. The usual objection — "3 roundtrips per intent is friction an agent routes around" — is answered by the tool design below: one call per intent, responses complete enough to need no follow-up. |
| **Tasks + events + notes** (three entities) | An agent resuming cold needs three different things: in-flight state (tasks), what happened and why (events), and durable knowledge about the code (notes). Notes have no lifecycle and outlive the task that produced them. Without them, knowledge is time-stamped narrative where superseded facts sit next to current ones with nothing marking which is which. |
| **Multi-project, scoped by call location** | One store, projects as rows. Cross-project becomes a `WHERE` clause instead of a federation problem. Cross-project recall ("I hit this same nginx thing somewhere before") is the feature no per-repo board can offer. |
| **Project resolved automatically**, `project` param optional | The client tells the server where it is (`CLAUDE_PROJECT_DIR`, then cwd) — no filesystem walk from an ambient cwd, which was a CLI assumption leaking into an MCP design. **Revised after `reference/INDEX.md`: SEP-2577 (Final) deprecates `roots/list`, so roots dropped from primary to opportunistic.** The param stays optional for cross-project queries and for HTTP, where there is no ambient context. It is not required, because an LLM generating an identifier is non-deterministic: `ai-kanban` today, `ai_kanban` tomorrow, and the board silently forks. Paths resolve deterministically; generated strings don't. Same reasoning that removed `session_id` in `ToolDesign.md`. |
| **Core / adapter split** | The human's access is *an API*, so a dedicated tool or web UI can be built later. Core returns structured data; the MCP adapter renders prose. Prose in core would force every future consumer to parse text back into objects. |
| **Rust**, single static binary | Nothing to install, nothing to keep updated, SQLite+FTS5 bundled. `ai-kanban mcp` today, `ai-kanban serve` (HTTP) later from the same binary — no second stack. This is what makes "little setup overhead" real rather than aspirational. |
| **Adoption mechanism: OPEN** | Explicitly deferred by the user ("we will see in the future"). `docs/adoption.md` lays out options and decision criteria; it does not pick one. |

---

## Footguns — audited against the primary goal

Each of these is a way this design could satisfy its own docs and still fail at "persistent memory
across many sessions" or lose to "agents shortcut." They are recorded here so the docs address them
rather than discover them later.

**1. No durability story for the one artifact holding all the value.** "Global DB, not in git" was
framed as a pure win. The flip side: not backed up, not versioned, doesn't travel between machines.
Desktop and laptop become two disjoint memories — the split-memory failure `project_paths` prevents,
recurring at machine level. A single SQLite file makes this trivial to fix and trivially easy to
never think about. `architecture.md` must state the backup path, an `export`/`import` pair on the
roadmap, and say plainly that sync is out of scope for v1 so it's a decision rather than an oversight.

**2. Adoption is load-bearing, not a nice-to-have.** The stated past failure is that agents skip the
board. Without a mechanism, nothing in this design does anything — a good tool surface makes adoption
*possible*, it does not cause it. `adoption.md` moves earlier in the sequence and says this outright,
so the docs don't read as though the tool surface is the solution.

**3. Memory that requires an explicit lookup goes unused.** `recall` assumes the agent thinks "let me
search my memory." It won't — it hits a bug and starts debugging. The valuable case is contextual
("you're editing auth middleware, here's what you learned last time"), which requires notes to be
associated with **files/paths**. This is an adoption question with a schema consequence: `notes` gets
a `paths` association in v1 even though nothing consumes it yet, because retrofitting it later means
re-tagging every note ever written.

**4. Notes rot, and confidently wrong memory is worse than none.** Nothing decides when a note stops
being true. Six months of notes about a changed codebase, injected as context an agent trusts, is
actively harmful. v1 minimum: `updated_at` on notes and `recall` showing hit age so the agent can
discount stale ones. Named as an open problem in `data-model.md`; not solved.

**5. Full-board-on-every-mutation doesn't scale — and an expensive tool is one agents avoid.**
`ToolDesign.md` returns the full list on every mutation, which is right for ~10 breakpoints and wrong
for a project with 200 tasks accumulated over a year. Repeatedly spending that many tokens per
`task_add` makes the board costly, and cost is what the shortcutting agent optimizes away. This is a
direct hit on the core failure mode. Mutation responses default to a bounded view — open tasks,
capped, with counts standing in for the rest — and `include` widens it.

**6. If memory does split, there is no recovery.** `project_paths` alias learning prevents most
splits, but nothing merges two projects that already diverged. Without a merge path the mitigation is
one-way and a split is permanent. Roadmap item, named in `data-model.md`.

**7. Project-level events need a named reader or they are write-only.** Nullable-`task_id` events are
justified only if something reads them. If the answer is "nothing, in practice," the agent is being
asked to write a log nobody consumes — exactly the extra work agents correctly avoid. The reader is
the `board` snapshot's `recent` section; state that, because it also bounds how much narrative
belongs in an event.

---

## The design law (from `ToolDesign.md`)

Every doc in this set is downstream of one principle, taken from the user's PhpStorm MCP project:

> A tool is not a thin wrapper for an API call, it is a refined tool — like an application for a
> human. **"Did the agent need knowledge about the API/system internals in order to use that tool?"**
> If yes, the tool is not good.

Concretely:

- **Natural language out, not JSON.** The agent isn't a REST client.
- **Three-part response**: Result (just the data, no narration) / Context (only when non-obvious) /
  Error (what went wrong + current state + what to do next).
- **Self-contained.** A bad response causes 3–5 follow-up calls. A good one causes zero.
- **Errors self-correct.** Not-found shows what *does* exist. Ambiguous lists the options with IDs.
- **One call per intent.** Consolidate where structure is identical; split where inputs differ
  fundamentally.
- **Input format matches output format.** Output from one tool is valid input to another.
- **Every argument we don't bother the agent with is a win.**

---

## Deliverables

Six files. Each has one job; none repeat another.

### 1. `CLAUDE.md` (repo root) — ~50 lines

How to work **on ai-kanban itself**. Every line is recurring token cost, so it stays tight.

- What the project is, the three priorities in order, non-goals in one line
- **The two-payload distinction** (see below) — stated explicitly so it can't blur
- The design law, compressed to the one-sentence test, pointing at `docs/tool-design.md`
- Rust conventions, `cargo` commands, where the DB lives in dev
- Dogfooding: ai-kanban's own board lives in ai-kanban once the server runs

> **The two-payload distinction.** There are two different instruction payloads and they must not
> merge:
> - `CLAUDE.md` at repo root = how to work on ai-kanban itself. Audience: a contributor.
> - The instructions ai-kanban eventually *ships* into consumer repos to make agents use the board =
>   a product artifact, tied to the deferred adoption decision. Audience: a consumer agent.
>
> Different audiences, different lifecycles. Conflating them is how this gets muddled on day one.

### 2. `docs/vision.md`

The what and the why. Written for someone with none of this conversation's context.

- The problem: knowledge dies at session end; the next session rediscovers it
- The three priorities, in order, each with what "done" looks like
- **Non-goals, stated hard** — this is the section that keeps the project honest:
  - not a platform, not an ecosystem, no orchestration shells, no per-task runtime
  - not a Jira clone, not a team planning tool
  - multi-agent is a schema property at most, never a subsystem
- Who the consumers are, in priority order: agent (MCP) → human (API/UI, later)

### 3. `docs/architecture.md` — short

- The layering, and why prose lives in the adapter rather than core:

  ```
  core  (SQLite + domain)   → structured data
    ├── MCP adapter          → prose for agents
    └── HTTP API (later)     → JSON for web UI / dedicated tools
  ```

- Single binary, subcommands: `ai-kanban mcp` (stdio, today), `ai-kanban serve` (HTTP, later)
- Store location: one global DB, platform data dir, created on first write. No init step.
- Concurrency: SQLite WAL. Multiple sessions across projects write concurrently; no daemon required.
- **Durability — say it out loud (footgun 1).** Everything of value lives in one file outside version
  control. The doc states where that file is, that backing it up is `cp`, that `export`/`import` are
  on the roadmap, and that multi-machine sync is **deliberately out of scope for v1**. Left unwritten
  this reads as an oversight; written down it is a decision, and the user finds out before the disk
  dies rather than after.
- v1 boundary: core is built with a clean structured API from day one, but only the MCP adapter
  ships. HTTP is designed for, not built.
- Crates to evaluate: `rmcp` (official Rust MCP SDK), `rusqlite` with `bundled`. **Verify FTS5 is
  enabled in the bundled build** — it is a compile flag, not a given.

### 4. `docs/data-model.md` — the load-bearing doc

**Schema** (DDL, pasteable into `sqlite3`):

- `projects` — `id, key, name, created_at`
- `project_paths` — `path, project_id`. A project owns **many** paths. This is what stops a worktree,
  a second clone, or a moved folder from silently forking the board.
- `tasks` — `id, project_id, title, body, status, type, origin, priority, blocked_by, created_at, updated_at`
- `events` — `id, project_id, task_id (nullable), ts, actor, kind, body`. Append-only. Nullable
  `task_id` means project-level history (decisions, session summaries) needs no task to attach to.
- `notes` — `id, project_id, task_id (nullable), title, body, tags, created_at, updated_at`
- `note_paths` — `note_id, path`. Associates a note with the files it is about. **Nothing consumes
  this in v1** — it exists because proactive contextual recall ("you're editing this file, here's
  what you learned about it") is the form of memory that actually gets used, and retrofitting the
  association means re-tagging every note ever written. Cheap now, expensive later. (Footgun 3.)
- FTS5 virtual tables over `notes`, and over `events`/`tasks` for recall

**Status values:** `backlog | doing | blocked | done | archived`. No `next` — priority covers it.
`blocked` earns its place because "why did this stall" is memory-relevant, and it pairs with
`blocked_by`. `archived` is terminal and hidden from the default board.

**`status` is authoritative, `blocked_by` is annotation.** The two can disagree (`status: blocked`
with `blocked_by: null` is legal — blocked on something outside the board). State this explicitly, or
every board render invents its own rule. `blocked_by` enriches the display; it never overrides
`status`.

**Note edits write an event.** `notes` holds current state only, but an in-place update would destroy
the record that a fact changed — on a board whose first priority is history, the knowledge layer is
the worst place to lose it. Every `note_update` writes an `events` row (`kind: note_updated`, actor,
previous value). Current state stays queryable in `notes`; the change stays in history with the rest
of history. The table already exists; this costs one insert.

**`origin` (`user | agent`)** — the instrumentation for priority #2. Without it you cannot observe
whether agent self-organization actually happens. The MCP adapter defaults it to `agent`; the agent
passes `user` when relaying a request the user made. Document the imprecision honestly: this
distinguishes "who typed it" better than "who thought of it", and it is the cheapest thing that makes
the feature measurable.

**Project resolution**, spelled out as an ordered rule so it is deterministic:

1. `CLAUDE_PROJECT_DIR` — the env var Claude Code sets in the spawned server's environment,
   documented as the stable project root. **SEP-2577 (Final) deprecates `roots/list`** and names
   environment variables as one of the explicit replacements, so this is primary, not a fallback.
2. Process cwd
3. `roots/list` — opportunistic only, if the client offers it. Deprecated; never build on it as the
   primary path. (See the warning in `reference/INDEX.md`.)
4. Look the path up in `project_paths`. Hit → done.
5. Miss → walk up to the git root; key on the git remote URL if there is one, else the root path.
   Create the project if new; **record the path as an alias** either way.
6. A `.ai-kanban` marker file containing a project key overrides the upward walk. This is the escape
   hatch for monorepo packages and non-git directories. Optional, never required.

Document the failure mode this rule exists to prevent: **if the same project resolves to two
identities, memory splits into two half-boards and neither is right — and nothing surfaces the
problem.** Alias learning (step 5) is the mitigation, and every response stating its resolved project
is the detection.

**Named as open, not solved** (footguns 4 and 6):

- **Note staleness.** Nothing decides when a note stops being true, and confidently wrong memory is
  worse than none. v1 ships `updated_at` and `recall` displaying hit age so the agent can discount
  old claims. Automatic staleness detection (flagging notes whose `note_paths` files changed
  substantially) is roadmap, not v1.
- **Project merge.** Alias learning prevents most splits; nothing repairs one that already happened.
  Until a merge path exists, the mitigation is one-way.

**Explicitly out of scope:** task dependencies beyond `blocked_by`, sprints, estimates, burndown,
assignees, WIP limits. Board-management ceremony that serves humans managing humans.

### 5. `docs/tool-design.md` — the agent surface

Opens with the design law above, then the **Board Snapshot**: the standard response shape most tools
return, the direct analog of `ToolDesign.md`'s Debug Snapshot. It always leads with the resolved
project, the way the Debug Snapshot always states the session — so the agent never speculates about
which board it is on.

```
Board: ai-kanban  (7 open, 2 doing)

doing
  #12 MCP tool surface            (user)
  #15 Project path resolution     (agent, blocked by #12)
backlog
  #18 FTS over notes              (agent)

recent
  2h  #12 → doing
  3h  note "roots vs CLAUDE_PROJECT_DIR"
```

**Response volume is a design constraint, not a detail (footgun 5).** `ToolDesign.md` returns the
full list on every mutation — correct for ~10 breakpoints, wrong for a year-old project with 200
tasks. Spending that on every `task_add` makes the board expensive, and cost is precisely what a
shortcutting agent optimizes away. So mutations return a **bounded** board by default: open tasks,
capped, with counts standing in for the remainder (`+31 backlog`, `+118 done`). `include` widens it
on request. The `recent` section is likewise capped. The doc states the caps.

**Tool surface — 8 tools.** Each is justified by an intent, and each intent is one call:

| Tool | Intent | Notes |
|---|---|---|
| `board` | "where do things stand" | Board Snapshot. `include`, `status`, `project` all optional |
| `task_add` | "I found something, file it" | The priority-#2 path. Must be one call, minimum friction |
| `task_update` | "move it, and here's why" | Status/priority/body **and** a `log` entry in one call. Returns the bounded board with `(updated)` |
| `task_show` | "resume this specific work" | One task, full event history, linked notes |
| `note_add` | "remember this about the code" | Durable knowledge |
| `note_update` | "that's no longer true" | Stale knowledge is worse than none, so notes must be correctable |
| `recall` | "what do we know about X" | FTS across notes/events/tasks. `project: "all"` for cross-project |
| `log` | "record what happened / what we decided" | Project-level event, no task needed. **Its reader is the `board` snapshot's `recent` section** — stated in the doc, because an append-only log nobody reads is exactly the extra work agents correctly avoid, and naming the reader also bounds how much narrative belongs in one event (footgun 7) |

**`recall` gets the same treatment as `board`** — a full section with a worked example, because it is
the tool priority #1 actually rests on. A store is only as good as its retrieval, and FTS5 over three
tables is where this design is most likely to disappoint in practice. The doc must fix:

- **Mixed result shapes.** A task hit, an event hit, and a note hit are not the same thing and must
  not render as if they were. Each hit states its kind.
- **Ranking and cap.** FTS5 `rank`, a default result limit, and a snippet window around the match —
  hits without surrounding context are titles, not answers.
- **Cross-project hits state their project on every line.** Otherwise the response is speculative in
  exactly the way the Board Snapshot header exists to prevent.
- **Empty results orient.** Per the design law, "no hits" must say what *is* in the store for this
  project and suggest widening to `project: "all"` — not just report nothing.

```
recall "redirect loop"  →

3 hits in ai-kanban

note  #7   "auth middleware rewrites redirects"
           …the middleware rewrites Location headers before the
           →redirect loop← guard runs, so the guard never fires…

task  #12  Fix auth redirect loop           (done, 2mo ago)
event      #12 → done: "root cause was middleware ordering,
           not the handler"
```

Deliberately absent, with reasons stated in the doc:

- No `task_remove` — `task_update(status: archived)` has identical structure. Consolidate.
- No `project_list` — `board(project: "all")` covers it, but **as a per-project summary, not a
  concatenated task list**. Every open task across every project is not a board. The doc specifies
  the summary shape (project, counts, what's in `doing`); dredging across projects is `recall`'s job.
- No required `project` arg on anything — see the `session_id` reasoning above.

The doc also fixes **notation** (one canonical format per domain, `#12` for tasks, reusable as input),
worked **response examples** for success / partial failure / not-found / ambiguous / empty state, and
**MCP tool annotations** (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) with
the reasoning per tool.

### 6. `docs/adoption.md` — the open problem

The user's stated main failure in the past: **agents avoid extra work and skip the board.** This doc
frames the problem and lays out mechanisms with trade-offs. It **does not pick one** — that decision
is deferred.

**It opens by saying this is the load-bearing part (footgun 2).** Without a mechanism, nothing else
in this design does anything: a good tool surface makes adoption possible, it does not cause it. The
rest of the docs describe a board that works beautifully once an agent decides to use it — this is
the doc about whether it ever does. It is written early, not last, so the doc set doesn't read as
though the tool surface is the answer.

It also covers the case `SessionStart` doesn't: **proactive contextual recall.** Board state at
session start is the easy half. The half that decides whether memory is used is surfacing a note when
it is relevant — the agent opens `auth/middleware.rs`, and what was learned about it last time
arrives without anyone asking. That is `PostToolUse`/`FileChanged`-shaped, not `SessionStart`-shaped,
and it is why `note_paths` exists in the v1 schema with no v1 consumer (footgun 3).

Options, weakest to strongest handed:

1. **Nothing** — rely on tool descriptions. The baseline that already failed.
2. **CLAUDE.md in the consumer repo** — cheap to ship, but agents skim standing instructions.
3. **A skill** — content persists in context for the session, but the model must still choose to
   invoke it.
4. **A plugin** — bundles skill + hooks + MCP server in one install. Best fit for "little setup".
5. **Hooks** — the only deterministic option. Highest leverage, most intrusive.

The finding worth writing down, because it is the strongest lever available and is not obvious:

> A **`SessionStart` hook returning `additionalContext`** puts the board into context *before the
> first prompt*. The agent does not have to choose to read it — which is the entire failure mode.
> A **`Stop` hook returning `hookSpecificOutput.additionalContext`** (not `decision: "block"`) nudges
> at end of turn without raising a hook error, and inherits the `stop_hook_active` flag and the
> 8-continuation cap as safety valves against nag loops.

Decision criteria to evaluate against, once there is something to measure:

- Does the agent read the board unprompted?
- Does it file tasks unprompted (`origin = agent`)?
- Does it record *why*, not just *what*?
- How annoying is it when the agent is doing something unrelated?

The tie-in worth stating: `events.actor` and `tasks.origin` make all of this queryable. **The schema
instruments its own adoption problem** — you can answer "is this working" with a SQL query instead of
a feeling.

**But state the caveat in the doc, or the first query will mislead.** `origin = agent` is a *floor,
not a measurement*. It defaults to `agent` unless the model remembers to pass `origin: user` when
relaying a request — and a shortcutting agent, which is precisely the population being measured, is
the one that won't bother. The count is biased in the flattering direction. The real signal is task
*content*: did the board fill with things nobody asked for?

---

## Sequencing

1. `docs/vision.md` — fixes scope and non-goals; everything else references it
2. `docs/adoption.md` — **moved up.** It is the highest-risk unknown and it has schema consequences
   (`note_paths`), so writing it before the data model means those consequences land in v1 instead of
   being retrofitted
3. `docs/data-model.md` — the load-bearing schema; the tool surface is downstream of it
4. `docs/tool-design.md` — the largest doc, and the one that carries the project's character
5. `docs/architecture.md` — short, mostly settled already
6. `CLAUDE.md` — written last, once there is something to point at

---

## Verification

Docs-only, so verification is a design review with teeth rather than a test run.

**Trace three scenarios end-to-end through the written docs.** Each must resolve to the stated number
of calls with nothing underspecified. If a scenario needs an extra call or a field that doesn't
exist, the docs are wrong and get fixed before this is done:

1. **Cold session resume** — agent opens a project it hasn't seen in a month. One `board` call must
   yield: in-flight work, why the blocked one is blocked, and what was recently learned.
2. **Side-quest capture** — agent is fixing A, spots unrelated bug B. One `task_add` call, no
   context switch, no follow-up to confirm it landed.
3. **Cross-project recall** — "have I hit this nginx redirect thing before?" One `recall` call with
   `project: "all"` returns hits with enough surrounding context to be useful, not just titles.

**Check the DDL actually runs.** Paste `data-model.md`'s schema into `sqlite3` and confirm it
executes, including the FTS5 virtual tables. A schema in a doc that doesn't run is a schema that will
be silently wrong when someone implements it.

**Apply the design law to every tool in the table.** For each: *"does the agent need internal
knowledge of ai-kanban to use this?"* Any yes is a redesign, not a note.

**Budget the responses at realistic scale.** Take a hypothetical year-old project — 200 tasks, 40
notes, 800 events — and estimate the token cost of `board`, of a `task_add` response, and of a
`recall` with 10 hits. If `task_add` is expensive at that scale, the design has recreated the exact
cost pressure that makes agents skip the board (footgun 5). Caps get tightened before this is done,
not after someone notices in six months.

**Walk the footgun list and confirm each is addressed in a doc, not just in this plan.** A footgun
recorded in a plan file nobody reads again is a footgun.

**Confirm the two payloads stayed separate.** `CLAUDE.md` should contain nothing a consumer agent
would need, and nothing in the docs should assume the adoption mechanism was decided.
