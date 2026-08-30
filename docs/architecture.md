# Architecture

```
core  (SQLite + domain)  ──> structured data
  │
  ├── render               ──> prose, shared by every agent-facing adapter
  │     ├── MCP adapter    ──> the tools an agent calls        (shipped)
  │     └── hook adapter   ──> context the agent never asked for (shipped)
  │
  └── HTTP API             ──> JSON + web UI, for a human      (shipped)
```

## Why prose lives outside core

Core returns structs and never a `String` of narrative. The human's access to this data is
meant to be an API, so text in core would force a future web UI to parse sentences back into
objects it already had. The rule has already paid for itself once: the hook adapter is a
second consumer of `render`, and adding it required no change to core at all.

## One binary

```
ai-kanban mcp                  MCP server on stdio -- what an agent connects to
ai-kanban hook session-start   Board as SessionStart context
ai-kanban hook post-tool-use   Notes about the file a tool just touched
ai-kanban where                Path to the store
ai-kanban serve [--port N]     Web UI + HTTP API on 127.0.0.1
```

Subcommands rather than separate binaries because "little setup overhead" is the pitch: one
file to install, one to update. `serve` landed here rather than in a second stack, and the
web UI is embedded in the binary for the same reason — a page that needs its assets beside it
is no longer one file.

The hook subcommands speak Claude Code's hook protocol directly — hook JSON in on stdin, hook
JSON out on stdout. `SessionStart` fires before the host finishes connecting to MCP servers,
so a hook on that event cannot call our own tools and has to be a command. Emitting the JSON
from the binary also means the plugin needs no wrapper script and no `jq`, and works on
Windows.

## The store

One global SQLite file, outside any repository. Location: the platform data directory,
overridable with `AI_KANBAN_DB`.

That override is not a convenience. Without it every test and every development run writes
into the developer's real memory store.

Created on first write. **There is no init step**, because an init step is setup overhead and
setup overhead is what stops a tool like this from being used.

Global rather than per-repo means no binary blob in git, no merge conflicts on a database,
and cross-project recall as a `WHERE` clause instead of a federation problem.

## Concurrency

SQLite in WAL mode. Multiple agent sessions across multiple projects write concurrently with
no daemon. Writers wait rather than fail (`busy_timeout`), because concurrent sessions are
the normal case here rather than the exception.

The MCP server serializes its own writes behind a mutex, so tool calls are individually
atomic. It handles requests concurrently, so a client that *pipelines* several calls without
awaiting each one may see them applied in a different order than sent. Real clients await
each response; the ordering is worth knowing about rather than defending against.

Hook subcommands open the store **read-only** and skip migrations entirely. They run in every
directory the user opens Claude Code in, under a host timeout, and must not be able to write,
migrate, or block. The cost of that is a store newer than the binary's read queries, which
`migrate::is_readable` turns into silence rather than an error.

Rows carry a `version` that every update bumps, and updates may pass the version they read
(`expected_version`) to make the write a compare-and-swap. The MCP path omits it — an agent's
read and write are milliseconds apart inside one tool call, and requiring it would turn every
update into two calls. The HTTP API always sends it, because a browser form open for minutes
is precisely the lost-update case. See `docs/http-api.md`.

## Schema versioning

`PRAGMA user_version`, with `src/core/schema.sql` frozen as the version-1 baseline and every
later change a numbered migration in `src/core/migrations/`.

Frozen matters: keeping the schema file current *and* writing a migration means every change
is written twice, and the two can disagree — at which point a fresh install differs from an
upgraded one, and the resulting bug depends on when the user first ran the tool.

Migrations run only from `Store::open`, each atomic with its own version stamp (SQLite has
transactional DDL, so a failed migration leaves the previous version rather than something
in between).

## Durability — stated out loud

**Everything of value is one file, outside version control.** It is not backed up, not
versioned, and does not travel between machines. Said plainly here so it is a decision rather
than something discovered after a disk failure.

- **Backup is `ai-kanban backup <file>`, not `cp`.**

  This doc previously said to copy the file. Under WAL that silently loses recent work: the
  newest writes sit in a `-wal` sidecar until a checkpoint, so a plain `cp` of `kanban.db`
  restores a board that is missing everything since the last checkpoint, with no error. It
  was found for real — a live store's main file was four days and six tasks behind its WAL.

  The advice then became `sqlite3 "$(ai-kanban where)" ".backup out.db"`, which is correct
  and still requires knowing what a WAL is, owning `sqlite3`, and getting a two-part shell
  incantation right. `backup` is the same operation (`VACUUM INTO`) behind a command that
  states its intent — the design law applied to the human rather than to the agent.

- **The store checkpoints when it closes.** `Drop for Store` runs `wal_checkpoint(TRUNCATE)`,
  best-effort. This does not make `cp` correct, and nothing should treat it as though it
  does; it narrows the window in which the naive copy that people and backup tools take
  anyway is a silent rollback. Failure is ignored: a busy checkpoint means another session
  will do it shortly, and the read-only hook connection cannot run one at all.

- **`export` / `import` are the portable pair.** JSON, readable and diffable, one project or
  all of them. `import` restores boards that are not present and **refuses** to merge into
  one that is — see `src/core/transfer.rs`. Choosing which side of a divergent history wins
  is task #3, and answering it halfway inside an importer would leave two half-answers.
- **Multi-machine sync: deliberately out of scope for v1.** A desktop and a laptop are two
  disjoint memories — the same split-memory failure `project_paths` prevents within a
  machine, recurring at machine level. Naming it is not solving it, but an unnamed version of
  this problem is the one that bites.

## Dependencies

Battle-tested crates by preference:

| Crate | Why |
|---|---|
| `rusqlite` (`bundled`) | The de-facto SQLite binding. Bundled means no system SQLite dependency. **FTS5 was verified in the bundled build, not assumed** — it is a compile flag. |
| `serde` / `serde_json` | Unavoidable, and the standard. |
| `tokio` | Required by the MCP transport. |
| `thiserror` | The standard error-derive. |
| `dirs` | Platform data directory. More widely used than the `directories` alternative. |
| `axum` | The HTTP server. Same maintainers as `tokio`, which was already a dependency, and the most-used Rust web framework. |
| `async-stream` | Writing the SSE stream as a generator rather than a hand-rolled `Stream` impl. |
| `schemars` | Tool input schemas, required by `rmcp`. |
| `rmcp` | **The deliberate exception.** New and not widely used, but it is the official Rust MCP SDK and there is no mature alternative. The core/adapter split is what limits the blast radius if it churns. |

No date/time crate: timestamps are unix integers and "2h ago" is arithmetic.

## What the layering actually bought

The claim was that core returning structs rather than prose would make a second consumer
additive. That is now tested rather than asserted: adding the whole HTTP API and web UI
required **no change to any existing core function** — only new ones (`core::page`), because
the old ones already returned objects.

The measured proof is `tests/budget.rs`: the agent's response costs are byte-identical before
and after the web UI exists. A second consumer cost the first one nothing.
