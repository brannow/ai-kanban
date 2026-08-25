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

**It must never fail loudly, or speak when it has nothing to say.** A hook that prints an
error on every session start gets deleted, and it takes the bundled MCP server with it.
Every failure path exits 0 in silence, and so does every directory with no board.

That second half was a deliberate reversal. A one-line "you could start a board here" nudge
looked harmless and solved the cold-start problem — a fresh project is otherwise completely
silent. But the store comes into existence the first time ai-kanban is used at all, so from
that point the line would appear in **every** directory, forever: `$HOME`, `/tmp`, an
unpacked tarball, someone else's clone. That is exactly the chattiness this document says
gets hooks uninstalled, and the agent can already see the tools exist from `tools/list`
without being told. Cold start is left to the tool descriptions.

---

## `PostToolUse` — contextual recall

Board state at session start is the easy half. The half that decides whether memory is
*used* is surfacing a note when it is relevant: the agent opens `auth/middleware.rs` and what
was learned about it last time arrives unasked. `recall` assumes the agent thinks "let me
search my memory", and it will not — it will hit a bug and start debugging.

This is what `note_paths` was in the schema for, and it now has its consumer.

Three things make the difference between help and irritation, since this fires on **every**
file read and edit:

**Path matching respects directory boundaries.** Stored paths are usually repo-relative
(`src/auth.rs`); the hook receives an absolute path. A stored path matches when it is the
absolute path, or a suffix of it *at a `/`*. Plain suffix matching would fire a note about
`auth.rs` on `vendor/other/auth.rs` — and a note surfacing on the wrong file is worse than no
note, because it is a confident claim about code it was never about.

**It never repeats itself.** Reading the same file five times must not deliver the same
paragraph five times. Notes already surfaced are recorded per session in a file in the OS
temp directory — not in the store, because the hook opens it read-only and writing "I
mentioned this" into the user's memory would make an observer into a participant. Losing that
state costs one repeated note, so every failure there is ignored rather than reported.

**It shows at most three notes and their age**, then a count. A file with a dozen notes
almost certainly has three that matter. Age is shown because staleness is an unsolved problem
and this is the one moment the agent can act on it — it is looking at the code the claim is
about — which is why the output ends by naming `note_update`.

Measured at ~0.5ms per invocation, and silent unless it has something to say.

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
> Making it distributable means shipping a built binary per platform.
>
> The obvious cheaper fix — point both configs at a bare `ai-kanban` and let `PATH` find it —
> was tried and reverted. `cargo install --path .` puts the binary in `~/.cargo/bin`, which
> is not on `PATH` on every machine (it is not on this one; rust here comes from Homebrew).
> A hook that cannot find its binary at all is strictly worse than one that only works when
> the plugin is discovered in place.

### Running it against this repo

Skills-directory discovery loads a plugin **in place** rather than copying it, which is
exactly what the in-place constraint needs:

```sh
cargo build --release
ln -sfn "$PWD" ~/.claude/skills/ai-kanban     # loads as ai-kanban@skills-dir next session
```

Undo with `rm ~/.claude/skills/ai-kanban`, or turn it off with
`claude plugin disable ai-kanban@skills-dir`. Discovery happens at session start, so it does
not appear in `claude plugin list` until the next session.

The MCP server config deliberately does **not** live in a root `.mcp.json`. That file is also
Claude Code's project-scoped MCP config, so a contributor working in this repo with the
plugin installed would get every tool registered twice.
