# Vision

## The problem

An agent finishes a session having worked something out — why a subsystem behaves the way it
does, which of three plausible fixes was the wrong one, what the actual root cause turned out
to be. None of that survives. The next session starts cold and rediscovers it, or worse,
doesn't, and repeats the mistake.

The knowledge was never written down because writing it down was extra work with no payoff
inside that session. That is the whole problem, and it is a structural one: the cost falls on
the session doing the work, and the benefit falls on a session that does not exist yet.

`ai-kanban` is a place for that knowledge to live, designed so that putting it there is
cheap enough to actually happen.

## What it is

A kanban board whose first-class consumer is an **agent**, not a human. A person can observe
and update it, but the agent is the primary user, and every design decision resolves in the
agent's favour when the two conflict.

**One exception, and it is a real amendment to priority #1.** A person can *forget* a task or
a note — permanently, along with its history. No agent can. It exists because the alternative
turned out to be worse: `update_note` keeps the previous body so superseded facts stay
recoverable, event bodies are searchable, and the result was that nothing could ever be
removed from the store at all. Overwriting a note preserved exactly what you were trying to
replace. A memory with no way to forget is not a feature, and the store is global across every
project on the machine. See `docs/http-api.md`.

That inversion is the point. A board built for humans optimises for at-a-glance overview,
drag-and-drop, and reporting. A board built for an agent optimises for: one call per
intention, responses complete enough to need no follow-up, and a token cost low enough that
using it never feels like a detour.

## Priorities, in order

The order matters. Where two priorities conflict, the higher one wins, and the lower one gets
dropped rather than compromised.

**1. Persistent memory and project history.**
An agent starting cold knows what is in flight, what happened before, and what was learned
about the codebase.

*Done looks like:* one `board` call after a month away yields the in-flight work, why the
blocked thing is blocked, and what was recently learned — with no follow-up call. One
`recall` answers "have I hit this before?", including on other projects.

**2. Agent self-organization.**
An agent that spots a side quest, a bug, or a TODO files it itself, without being told.

*Done looks like:* the board fills with tasks nobody asked for, and they are useful ones.
This is instrumented (`tasks.origin`, `events.actor`) but the instrumentation is a floor, not
a measurement — see `docs/adoption.md`.

**3. Multi-agent orchestration.**
Minor to purely optional. **Not designed for.** At most a schema property; never a subsystem.

## Non-goals

Stated hard, because the failure mode this project is reacting against is real: previous
attempts at this problem became entire platforms with orchestrated shells per task. Whole
ecosystems, to solve "remember what happened last week".

- **Not a platform, not an ecosystem.** No orchestration layer, no per-task runtime, no
  scheduler, no daemon.
- **Not a Jira clone.** No sprints, estimates, burndown, assignees, WIP limits, or workflow
  states beyond the five that carry meaning. Board-management ceremony serves humans managing
  humans.
- **Not a team planning tool.** One person and their agents.
- **Not a chat log.** Events record what happened and why, not a transcript.
- **Not cross-platform.** macOS and Linux. Windows support means platform code written
  blind on a machine that cannot run it, and shipping that is worse than not claiming it.

The test for any proposed feature: does an agent starting cold work better because of it? If
the honest answer is "it would look more complete", it does not belong.

## Consumers, in priority order

1. **The agent**, over MCP. This is what exists.
2. **A human**, over an HTTP API — a web UI or a dedicated tool. Designed for, not built. It
   is why core returns structured data and prose lives in an adapter: text in core would
   force every future consumer to parse sentences back into objects.

## Why this can stay small

One SQLite file, one static binary, no daemon, no server, no per-project setup. The entire
system is a schema, a few thousand lines of Rust, and the discipline to keep saying no.

That is not modesty. Setup overhead is what kills tools like this: anything that needs
configuring per project will not get configured, and anything not configured is not used.
