# Tool design

## The design law

Everything here is downstream of one principle:

> A tool is not a thin wrapper for an API call. It is a refined tool, like an application
> made for a human. The test: **did the agent need knowledge about the system's internals in
> order to use it?** If yes, the tool is not good.

Concretely:

- **Natural language out, not JSON.** The agent is not a REST client.
- **Self-contained responses.** A bad response causes three to five follow-up calls. A good
  one causes zero.
- **Errors self-correct.** Not-found shows what *does* exist. An invalid value lists the
  valid ones. Ambiguity lists the candidates.
- **One call per intent.** Consolidate where the structure is identical; split where the
  inputs differ fundamentally.
- **One tool's output is another's input.** `#12` on a board is what you pass to `task_show`.
- **Every argument we don't bother the agent with is a win.**

The last one is why only `title`, `query`, `body` and the ids are ever required. Everything
else has a defensible default.

---

## The Board Snapshot

The standard response shape. Most tools return it.

```
Board: docproj  (3 open, 1 doing)

doing
  #1    MCP tool surface                         (user)

blocked
  #2    Project path resolution                  (agent, blocked by #1)

backlog
  #3    FTS over notes                           (agent)

recent
  just now decided to key projects on the normalized git remote URL
  just now note "roots vs CLAUDE_PROJECT_DIR"
  just now #3 filed: FTS over notes
```

**It always leads with the resolved board.** Not decoration: the store is global and holds
every project, so an agent that does not know which board it is looking at can file work onto
the wrong one. Stating it on every response is also the *detection* mechanism for a project
resolving to two identities — the failure that otherwise happens silently.

**`origin` is always shown.** It is the instrumentation for "does the agent file work
unprompted", and a metric nobody sees is a metric nobody checks.

**The `recent` section is the reader for project-level events.** An append-only log with no
named consumer is exactly the extra work an agent correctly skips. Naming the reader also
bounds how much narrative belongs in one entry: if it doesn't fit on that line, it's a note.

---

## Response volume is a design constraint

Not a formatting detail. The obvious implementation returns the whole board on every
mutation, which is correct at ten tasks and unaffordable at two hundred — and an expensive
board is one a shortcutting agent routes around, which is the exact failure this project
exists to fix.

Measured on a year-old board (200 tasks, 40 notes, 800 events), asserted in `tests/budget.rs`:

| Call | Cost |
|---|---|
| `board` | ~611 tokens |
| `task_add` | ~127–138 tokens |
| `recall`, 10 hits | ~510 tokens |
| `task_show`, 16 events | ~358 tokens |
| `board(project: "all")`, 60 boards | ~509 tokens |

The `task_add` figure is the one that matters — it is paid every time the agent files a side
quest, the behaviour this project most wants to encourage.

It started at 289 tokens, most of it a dozen unrelated backlog rows. **A write response now
shows in-flight work plus the task that changed**, because a backlog slice answers neither
question a write response should: "did it land?" and "what else is going on?" The backlog is
still reported as a count. Nothing is ever dropped silently — `...+29 backlog` is the
difference between a board showing a slice and a board hiding work.

---

## The eight tools

| Tool | The intent it serves |
|---|---|
| `board` | "Where do things stand?" |
| `task_add` | "I found something, file it." |
| `task_update` | "Move it, and here's why." |
| `task_show` | "Resume this specific piece of work." |
| `note_add` | "Remember this about the code." |
| `note_update` | "That's no longer true." |
| `recall` | "What do we know about X?" |
| `log` | "Record what happened / what we decided." |

Each is one call. `task_update` takes status, priority, body **and** the `log` entry
together, because splitting them would make recording the reason a separate call — and a
separate call is the one that gets skipped.

### Workstreams add no tool

Migration 005 added workstreams — named slices of work inside a board. It added **zero
tools**, and that is the design rather than an economy.

"Manage a workstream" is not an intent an agent has. It is bookkeeping, and the table above
maps tools to intents. So workstreams ride the intents that already exist:

| The agent's intent | What happens |
|---|---|
| "Where do things stand?" | `board()` renders the current workstream plus a one-line directory of the others |
| "Show me the contact-form work" | `board(workstream: "contact-form")` — **looking at a workstream is entering it** |
| "I found something, file it" | `task_add(title)` inherits the current workstream, with **no new argument** |

| "This belongs to different work" | `task_update(workstream: …)` — `""` moves it out of every workstream |

The `task_add` row is load-bearing. Per *"every argument we don't bother the agent with is a
win"*, and because filing is the call an agent under pressure skips, a workstream the agent
must remember to supply is one that ends up unset.

The `task_update` row is the consequence of that choice. Because filing inherits **silently**,
mis-filing is the expected error rather than an edge case — so the move has to exist, and it
belongs on `task_update` ("move it, and say why") rather than on a tool of its own. Without a
`log`, the automatic summary names the destination workstream, because an id in the history
would be unreadable six months later.

State-as-a-side-effect-of-use has precedent: `add_path_alias` learns a path because a board
was *used*, not through a call of its own. But the analogy has one sharp edge, and it is why
`board` prints `Now working in: X` when the scope changes. Alias learning **converges** — the
same directory always learns the same board. Entering-by-looking does not: an agent glancing
at an adjacent workstream has silently changed where its next `task_add` lands. So the switch
announces itself rather than relying on the agent noticing a changed header.

For the same reason `board` is annotated **`read_only_hint = false`**. A `board` call carrying
a `workstream` is not a pure read — it records where work is happening, and every later
`task_add` inherits it. Annotating it read-only would be convenient and untrue, and the client
that trusts the annotation is exactly the one that gets surprised. Plain `board()` writes
nothing, but annotations cannot be conditional, so it describes the wider case.

The bound on all of this: a tool may do more **work** internally, never return more **text**.
The directory is one line with counts however many workstreams exist. Rendering each
workstream's tasks is the "grouped rendering" this design rejected — it would force
`compute_omitted` to account per group or the *nothing is dropped silently* guarantee starts
lying.

### `task_show`

```
Board: docproj

#2 Project path resolution
  blocked, task, normal priority, agent, filed just now

blocked by #1 MCP tool surface (doing)

history
  just now #2 filed: Project path resolution
```

The deliberate exception to the volume rule: full history, because it is asked for only once
the agent has committed to one piece of work, and history is what it came for.

When `status` is `blocked` but nothing on the board blocks it, the response says so
explicitly. `status` is authoritative and `blocked_by` is annotation; they are allowed to
disagree, and silence would read as a missing field rather than a deliberate state.

---

## `recall`

The tool priority #1 actually rests on. A store is only as good as its retrieval, and this is
where the design is most likely to disappoint in practice — everything can be stored
correctly and still be unfindable.

```
1 hit for "redirect loop" in all projects

note  #2    nginx proxy_pass drops the trailing slash  (docproj-b, just now)
       A trailing-slash mismatch makes nginx issue a 301 to the absolute URL,
       producing a >>redirect<< >>loop<< behind TLS termination.
```

**Hits keep their kind.** A note is a durable claim about the code, a task is work, an event
is one moment. Flattened into an undifferentiated list they stop answering the question that
was asked.

**Hits carry a snippet.** A list of titles is a search result; a list of snippets is an
answer, and the difference decides whether the agent makes three more calls.

**Cross-project hits name their project on every line** — not just in the header. Otherwise
the result is speculative in exactly the way the board header exists to prevent.

**Task hits state their status**, so "already solved, two months ago" is visible without a
follow-up.

**No search syntax is required.** FTS5's `MATCH` is a query language: `auth-loop` is a NOT
expression, a stray quote is a syntax error, `AND` is a keyword. Raw input would make natural
phrasings fail with a parser error — and the agent would have to learn FTS5 to avoid it,
which is precisely the "needs internal knowledge" failure the design law forbids. Every token
is quoted and ANDed; bare operators are dropped as noise.

### Empty results orient

```
No hits for "kubernetes" in docproj.

This board holds 1 note, 4 tasks and 6 events.
Pass project: "all" to search the other 1 board.
```

"No hits" alone causes a follow-up call. Saying what the store *does* hold, and that a wider
search exists, turns a dead end into a next step.

---

## Errors

```
No task #999 on board "docproj".

On this board:
  #4 config loader ignores env overrides
  #3 FTS over notes
  #2 Project path resolution
  #1 MCP tool surface
```

```
"finished" is not a valid status.

Valid values: backlog, doing, blocked, done, archived
```

Both are one call away from correct. The listing is scoped to the resolved board — showing
another project's tasks would suggest ids that still do not work, and give no clue why.

Ambiguity is never resolved silently. A `project` that matches two boards lists both and asks;
picking one would write to a board the agent did not mean, and nothing would surface it.

---

## `project` is optional everywhere

It resolves from where the caller actually is (`CLAUDE_PROJECT_DIR`, then cwd).

Requiring the agent to name the project looks harmless and is not: a generated identifier is
non-deterministic — `ai-kanban` today, `ai_kanban` tomorrow — and the board silently forks
into two half-memories with nothing to report it. Paths resolve the same way every time.

The parameter stays available for deliberate cross-project work: `project: "all"` searches
every board with `recall`, and summarizes them with `board`.

---

## Deliberately absent

**No `task_remove`.** `task_update(status: "archived")` has identical structure. Two tools
with the same shape is a tool that should have been one.

**No `project_list`.** `board(project: "all")` covers it — but as a **per-project summary,
not a concatenated task list**:

```
2 boards

docproj  (4 open, last active just now)
  doing  #1 MCP tool surface

docproj-b  (0 open, last active just now)
```

Every open task across every project is not a board, it is a pile. Digging across projects is
`recall`'s job. Capped at 25 boards, most recently active first, because this is the one call
whose cost scales with something the project does not control: how many repositories the user
has touched all year.

**No required `project` argument.** See above.

**No tool for reading raw events.** They surface through `board`'s `recent`, `task_show`'s
history, and `recall`. A tool that dumps the log would be a tool for producing token cost.

---

## Notation

One canonical format per domain, and every one of them is valid input somewhere else.

| Form | Means | Accepted by |
|---|---|---|
| `#12` | Task | `task`, `blocked_by` |
| `#7` (note context) | Note | `note` |
| `backlog\|doing\|blocked\|done\|archived` | Status | `status` |
| `low\|normal\|high\|urgent` | Priority | `priority` |
| `"all"` | Every board | `project` |
| `0` | Clear `blocked_by` | `blocked_by` |

That last one is a compromise worth naming: JSON has no way to say "set this to null" that
survives an optional field, and inventing a magic string would be worse than a documented
sentinel.

---

## MCP annotations

| Tool | `readOnlyHint` | `idempotentHint` | `destructiveHint` |
|---|---|---|---|
| `board`, `task_show`, `recall` | true | true | — |
| `task_add`, `note_add`, `log` | false | false | false |
| `task_update`, `note_update` | false | false | false |

`destructiveHint` is false throughout because nothing is ever destroyed: `archived` is a
status rather than a delete, and a note edit writes the previous value into the event log.

The write tools are **not** idempotent, and saying so matters: calling `task_add` twice
creates two tasks. A client that assumed otherwise and retried would silently duplicate work.
