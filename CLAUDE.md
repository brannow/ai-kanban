# Working on ai-kanban

A kanban board whose first-class consumer is an **agent**, not a human. It exists so that
knowledge does not die at the end of a session.

Priorities, in order. Where they conflict, the higher one wins:

1. **Persistent memory and project history** — an agent starting cold knows what is in
   flight, what happened before, and what was learned about the codebase.
2. **Agent self-organization** — an agent that spots a bug or a side quest files it itself.
3. **Multi-agent orchestration** — not designed for. A schema property at most, never a
   subsystem.

Not a platform, not a Jira clone, not a team tool. See `docs/vision.md` for the non-goals,
which are load-bearing.

## Two payloads, never merged

- **This file** = how to work on ai-kanban. Audience: a contributor.
- **What the plugin ships into consumer repos** (`GUIDANCE` in `src/hook.rs`) = what makes an
  agent use a board. Audience: a consumer agent.

Different audiences, different lifecycles. Conflating them makes both wrong.

## The design law

> Did the agent need knowledge of ai-kanban's internals to use this tool? If yes, it is not
> a good tool.

Apply it to every change to the tool surface. Details and worked examples in
`docs/tool-design.md`.

## Where the real decisions are written down

| Doc | What it settles |
|---|---|
| `docs/data-model.md` | Schema and why each column exists. Read before changing `schema.sql`. |
| `docs/tool-design.md` | The agent-facing surface and its response shapes. |
| `docs/adoption.md` | Hooks and the plugin — the load-bearing risk. |
| `docs/http-api.md` | The human-facing API, the web UI, and live updates. |
| `docs/architecture.md` | Layering, the store, durability, dependencies. |
| `docs/plan.md` | Founding record. Superseded by the above where they disagree. |

## Conventions

- **Core returns structs, never prose.** Rendering lives in `src/render.rs`. Text in core
  would force every future consumer to parse sentences back into objects.
- **Task and note lookups take a `project_id`.** The store is global; a bare id lookup can
  read or overwrite another board's row.
- **Every `ORDER BY` on a timestamp needs an `id` tie-break.** Timestamps are whole seconds,
  so rows written in the same second tie and order arbitrarily.
- **`schema.sql` is frozen at v1. Schema changes are migrations** (`src/core/migrations/`,
  registered in `src/core/migrate.rs`). Editing both the schema file and adding a migration
  writes every change twice and lets a fresh store drift from an upgraded one.
- **A column list used by a shared `row_to_*` lives in exactly one place.** `TASK_COLS` was
  once duplicated across two files that fed the same row mapper; adding a column to one
  shifts every index after it and reads fields into the wrong struct members.
- **Response caps are asserted in `tests/budget.rs`.** Widening one without updating that
  test is how the board quietly becomes too expensive to use.
- **Every mutation writes an event.** The events table is also the change feed the HTTP live
  stream polls (`MAX(events.id)`), so a write with no event is invisible to a live page.
  `tests/change_feed.rs` asserts it for every mutation. Infrastructure events go in
  `HOUSEKEEPING_KINDS`, which keeps them out of the agent's `recent` without hiding them
  from the feed.
- **Hook code must never write, never create, and never fail loudly.** It runs in every
  directory the user opens Claude Code in.
- Comments explain *why*. What the code does is already visible.

## Commands

```sh
cargo test                      # 110 tests
cargo test --test budget -- --nocapture   # prints measured token costs
cargo build --release           # the plugin's hook and MCP configs both need this
AI_KANBAN_DB=/tmp/x.db ./target/release/ai-kanban serve   # web UI on :7373
claude plugin validate .claude/skills/ai-kanban   # the plugin is project-local
```

**Set `AI_KANBAN_DB` when running anything by hand.** Without it you are writing to your real
memory store. `ai-kanban where` prints the path.

## Dogfooding

ai-kanban's own board lives in ai-kanban. If you are working here with the plugin enabled,
file what you find and record why things changed — this repo is the first test of whether any
of the adoption reasoning survives contact.
