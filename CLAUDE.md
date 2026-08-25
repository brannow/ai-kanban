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
| `docs/architecture.md` | Layering, the store, durability, dependencies. |
| `docs/plan.md` | Founding record. Superseded by the above where they disagree. |

## Conventions

- **Core returns structs, never prose.** Rendering lives in `src/render.rs`. Text in core
  would force every future consumer to parse sentences back into objects.
- **Task and note lookups take a `project_id`.** The store is global; a bare id lookup can
  read or overwrite another board's row.
- **Every `ORDER BY` on a timestamp needs an `id` tie-break.** Timestamps are whole seconds,
  so rows written in the same second tie and order arbitrarily.
- **Response caps are asserted in `tests/budget.rs`.** Widening one without updating that
  test is how the board quietly becomes too expensive to use.
- **Hook code must never write, never create, and never fail loudly.** It runs in every
  directory the user opens Claude Code in.
- Comments explain *why*. What the code does is already visible.

## Commands

```sh
cargo test                      # 61 tests
cargo test --test budget -- --nocapture   # prints measured token costs
cargo build --release           # the plugin's hook and MCP configs both need this
claude plugin validate .
```

**Set `AI_KANBAN_DB` when running anything by hand.** Without it you are writing to your real
memory store. `ai-kanban where` prints the path.

## Dogfooding

ai-kanban's own board lives in ai-kanban. If you are working here with the plugin enabled,
file what you find and record why things changed — this repo is the first test of whether any
of the adoption reasoning survives contact.
