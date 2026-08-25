# Adoption

Everything else in this project makes the board *possible* to use. This document is about
whether anything ever actually uses it — which is the part that decides if the rest matters.

The stated failure from previous attempts is that **agents skip the board**. That is not
fixed by a better tool surface. Calling a tool is a choice, made under time pressure, by
something optimising for the shortest path to an answer. Recording a decision for a future
session is exactly the kind of work that gets optimised away, and it is invisible when it
does — nobody notices the note that was never written.

So adoption is the load-bearing risk, not a finishing touch.

---

## Why a hook

The options, weakest to strongest:

| Mechanism | Why it is not enough |
|---|---|
| Tool descriptions alone | The baseline that already failed |
| `CLAUDE.md` in the consumer repo | Cheap to ship, but standing instructions get skimmed |
| A skill | Persists for the session, but the model still has to choose to invoke it |
| A plugin | Bundles everything into one install — a delivery mechanism, not a motivator |
| **A hook** | The only option that does not depend on the agent choosing |

A `SessionStart` hook returning `additionalContext` puts the board into context **before the
first prompt**. The agent does not decide to read it. That is the entire failure mode,
removed — not mitigated.

---

## What ships today

**`SessionStart` only.** The hook emits the board plus a short statement of what the board
is for, and the plugin bundles the MCP server alongside it so there is a single install and
no manual MCP wiring.

### Three constraints from the host that shaped the design

Each of these was found in `reference/hooks.md` and each would have produced a broken or
useless hook if assumed the other way:

**`SessionStart` fires before MCP servers finish connecting** (`hooks.md:549`). A hook on
this event therefore *cannot* call ai-kanban's own MCP tools. It has to be a command — which
is why the binary itself speaks the hook protocol (`ai-kanban hook session-start`, hook JSON
in on stdin, hook JSON out on stdout). That also means no wrapper script, no `jq`
dependency, and it works on Windows.

**`FileChanged` is the wrong event for contextual recall.** Its `matcher` builds a *literal
filename watch list* in the working directory (`hooks.md:2764`) — it exists for config files,
not for noticing which source file the agent just opened. `PostToolUse` matching `Read|Edit`
is the right shape, since it receives `tool_input.file_path`.

**Hook output is capped at 10,000 characters** (`hooks.md:893`), and past it the text is
replaced with a preview and a file path. The board's own caps are what keep this comfortable;
the worst case measured in `tests/hook.rs` is under 3,000. That test exists so widening a
board cap fails here rather than silently truncating a board mid-row in production.

### Two rules the hook must never break

**It must not create anything.** The hook runs in every directory the user opens Claude Code
in. If it resolved projects the way the MCP server does, it would mint a board for every
scratch folder, tarball and dotfiles checkout — turning the store into a record of where the
user has been rather than what they work on. Hence `find_project` and `open_existing`, which
look up and never insert.

**It must never fail loudly.** A hook that prints an error on every session start gets
deleted, and it takes the bundled MCP server with it. Every failure path exits 0 in silence.
The one exception is a user who has a store but no board for this directory: they get a
single line saying a board would persist, because complete silence gives a fresh project no
reason to ever start using one.

---

## Deliberately deferred

**`PostToolUse` contextual recall.** Board state at session start is the easy half. The half
that decides whether memory is *used* is surfacing a note when it is relevant — the agent
opens `auth/middleware.rs` and what was learned about it last time arrives unasked. `recall`
assumes the agent thinks "let me search my memory", and it will not; it will hit a bug and
start debugging.

This is why `note_paths` exists in the v1 schema **with no consumer**. Retrofitting the
association would mean re-tagging every note ever written, so it ships now and waits.

**A `Stop` hook nudge** at end of turn ("did you record what happened?"). Returning
`hookSpecificOutput.additionalContext` rather than `decision: "block"` nudges without raising
a hook error, and inherits `stop_hook_active` and the continuation cap as safety valves. It
is the most likely mechanism to actually fill the board, and the most likely to irritate
someone into disabling the whole plugin. Ship the quiet thing first, see whether the board
fills, add this only if it does not.

---

## How to tell whether it worked

`events.actor` and `tasks.origin` make this a query rather than a feeling:

```sql
SELECT origin, COUNT(*) FROM tasks GROUP BY origin;
```

**But read `origin = agent` as a floor, not a measurement.** It defaults to `agent` and only
becomes `user` when the model remembers to say so — and the shortcutting agent this metric
is meant to detect is precisely the one that will not bother. The count is biased in the
flattering direction.

The honest signals:

- Did the board fill with things nobody asked for? (task *content*, not the `origin` column)
- Do task updates carry a `log`, or only a status change?
- Do notes get corrected when they go stale, or just accumulate?
- How often is the hook's output irrelevant to what the session was actually about?

---

## Installing

The plugin lives at the repo root: `.claude-plugin/plugin.json`, with its configs in
`plugin/`. Both the hook and the MCP server run the same binary, so it has to exist first:

```sh
cargo build --release          # produces target/release/ai-kanban
claude plugin validate .
```

> **Today this plugin is in-place only, not copy-installable.**
>
> Both configs point at `${CLAUDE_PLUGIN_ROOT}/target/release/ai-kanban`, and `target/` is
> gitignored — so any install that *copies* the repo (a marketplace entry,
> `claude plugin install`) lands without the binary. Discovery in place works, which is why
> validation and local testing pass and hide this.
>
> The failure is also asymmetric. The hook degrades silently by design, but a plugin MCP
> server that cannot spawn surfaces a visible connection error — so the "never fail loudly"
> rule does not cover the half that actually breaks.
>
> Making it distributable means shipping a built binary per platform, or pointing both
> configs at an `ai-kanban` already on `PATH` (`cargo install --path .`) and accepting that
> the plugin then depends on a separate install step.

The MCP server config deliberately does **not** live in a root `.mcp.json`. That file is also
Claude Code's project-scoped MCP config, so a contributor working in this repo with the
plugin installed would get every tool registered twice.
