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
in on stdin, hook JSON out on stdout). That also means no `jq` dependency and no shell
pipeline parsing hook JSON. There is a wrapper script, but it only locates the binary and
`exec`s it -- see *Installing* -- so the protocol still has exactly one implementation.

**`FileChanged` is the wrong event for contextual recall.** Its `matcher` builds a *literal
filename watch list* in the working directory (`hooks.md:2764`) — it exists for config files,
not for noticing which source file the agent just opened. `PostToolUse` is the right shape,
since it receives `tool_input.file_path`.

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

**The matcher is `Read|Edit|Write|Bash`, and `Bash` is the half that was missing.** The
obvious matcher is `Read|Edit|Write`, and it was wrong for a reason that took dogfooding to
see: agents mostly do not read files with `Read`. This project's own `CLAUDE.md` tells them to
use `cat`, `sed` and `grep`, and Claude Code's auto mode does too. So contextual recall fired
on a minority of reads and stayed silent on the rest — the worst possible failure for a memory
system, because nothing indicates it is being skipped.

`Bash` gets no `file_path`, only `command`. The fix is to pull candidate paths out of the
command string and keep the ones that resolve to real files. The parsing is deliberately
crude — split on whitespace and shell separators, drop flags, discard anything that is not a
file on disk. **The existence check is the only filter that matters**; understanding shell
properly would be a lot of code to slightly reduce the number of `stat` calls, and would still
be wrong on the first construct nobody anticipated. Measured at ~4ms per invocation, bounded
to 40 tokens and 4 files so a heredoc cannot turn into work.

Three things make the difference between help and irritation, since this fires on **every**
file read, edit, and shell command:

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

The plugin lives at `.claude/skills/ai-kanban/`, **inside this repository**. A folder in a
project's `.claude/skills/` that contains a `.claude-plugin/plugin.json` loads as
`ai-kanban@skills-dir` (`reference/skills.md:150`), gated by the workspace trust dialog.

So the only setup for this repo is:

```sh
cargo build --release
```

The plugin is discovered at session start, so it does not appear in `claude plugin list`
until the next session. Turn it off with `claude plugin disable ai-kanban@skills-dir`.

### Project-local here, personal everywhere else

Both locations work, and they answer different questions:

| Location | Applies to | Trust dialog |
|---|---|---|
| `~/.claude/skills/ai-kanban/` | every project | no |
| `<repo>/.claude/skills/ai-kanban/` | that repo | yes, once |

This repository uses the project-local one, because the plugin is part of the thing being
developed and should travel with it in version control. That is a dogfooding choice, not a
constraint.

It **was** a constraint, and the note is worth keeping because the reasoning is easy to
re-derive wrongly. Before the resolver (task #1), both configs named
`${CLAUDE_PLUGIN_ROOT}/../../../target/release/ai-kanban`, so the plugin directory had to
sit inside the checkout for the binary to be reachable at all. A global install would have
meant symlinking the whole repository into `~/.claude/skills` — a 757MB `target/`, 208
vendored reference files and `.git`, in a directory scanned at session start — and
symlinking it *inside* the repo would be a loop. The resolver removed that constraint
entirely; nothing about the layout depends on it any more.

### How the binary is found

`bin/ai-kanban` in the plugin is a resolver: it searches for the real binary and `exec`s it
with the arguments untouched. Both `hooks.json` and `mcp.json` invoke it, since
`${CLAUDE_PLUGIN_ROOT}` is set for both.

It searches, in order: `$AI_KANBAN_BIN`; `target/release` then `target/debug` relative to a
surrounding checkout; `~/.local/bin`; `~/.cargo/bin`; the Homebrew and `/usr/local` bin
directories; and finally `PATH`.

Those directories are named explicitly rather than left to `PATH` because **a hook is not a
login shell** — the host may invoke it with a minimal environment containing none of the
user's directories. Anything the README tells someone to install into therefore has to be on
this list, or the plugin works when they type the command themselves and silently does
nothing in a session. That failure is close to undiagnosable from the outside, so
`tests/plugin_install.rs` asserts the documented location resolves with `PATH` stripped to
`/usr/bin:/bin`.

Two of those orderings are deliberate. **The checkout comes before `PATH`** so that
developing ai-kanban tests the build you just made rather than a global install silently
shadowing it. **`PATH` comes last and is skipped if it resolves back into the plugin's own
`bin/`**, because a plugin directory on `PATH` would otherwise make the script re-exec
itself forever.

This replaced a hard-coded `${CLAUDE_PLUGIN_ROOT}/../../../target/release/ai-kanban`, which
only resolved when the plugin sat inside a checkout — so a copied plugin, which is how
plugins are normally installed, pointed at nothing. The cheaper fix of naming a bare
`ai-kanban` on `PATH` was tried before and reverted: `cargo install` puts it in
`~/.cargo/bin`, which is not on `PATH` on every machine (it is not on this one; rust here is
from Homebrew). Searching several locations is what makes both setups work at once.

When nothing is found, `hook` exits 0 in silence — a hook that complains on every session
start is a hook the user removes, taking the bundled MCP server with it. Every other
subcommand explains what to install, because an MCP server that dies without a reason is a
board that is mysteriously absent.

> **Windows is not supported, by decision.** The resolver is a `#!/bin/sh` script, so the
> hooks and the MCP server need a POSIX shell. A `bin/ai-kanban.cmd` shim with the same
> search order would fix it, and it is deliberately not being written: there is no Windows
> machine here, so it would ship untested, and untested platform code is a support burden
> that reads as a promise. The binary itself builds and runs on Windows — it is the plugin
> entry point that does not.

## `instructions` — the channel that needs no hook

The MCP server sends `instructions` at initialize. They reach the model without any hook
firing, being enabled, or being chosen, so they are what remains if the hooks are ever turned
off. They say the same thing as the `SessionStart` guidance in fewer words, rather than
describing the API — the client already has the tool list for that.

The MCP server config deliberately does **not** live in a root `.mcp.json`. That file is also
Claude Code's project-scoped MCP config, so a contributor working in this repo with the
plugin installed would get every tool registered twice.
