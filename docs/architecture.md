# Architecture

```
core  (SQLite + domain)  ──> structured data
  │
  ├── render               ──> prose, shared by every agent-facing adapter
  │     ├── MCP adapter    ──> the tools an agent calls        (shipped)
  │     └── hook adapter   ──> context the agent never asked for (shipped)
  │
  └── HTTP API             ──> JSON for a web UI               (designed for, not built)
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
```

Subcommands rather than separate binaries because "little setup overhead" is the pitch: one
file to install, one to update. `ai-kanban serve` (HTTP) will land here rather than in a
second stack.

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

Hook subcommands open the store **read-only** and skip the schema batch entirely. They run in
every directory the user opens Claude Code in, under a host timeout, and must not be able to
write, migrate, or block.

## Durability — stated out loud

**Everything of value is one file, outside version control.** It is not backed up, not
versioned, and does not travel between machines. Said plainly here so it is a decision rather
than something discovered after a disk failure.

- **Backup is a copy:** `cp "$(ai-kanban where)" ~/backups/`
- **`export` / `import`:** roadmap.
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
| `schemars` | Tool input schemas, required by `rmcp`. |
| `rmcp` | **The deliberate exception.** New and not widely used, but it is the official Rust MCP SDK and there is no mature alternative. The core/adapter split is what limits the blast radius if it churns. |

No date/time crate: timestamps are unix integers and "2h ago" is arithmetic.

## The v1 boundary

Core is built with a clean structured API from day one, but only the MCP and hook adapters
ship. HTTP is designed for, not built — and "designed for" means the layering makes it
additive, not that a stub exists.
