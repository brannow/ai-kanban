# ai-kanban

A kanban board an agent uses as memory. What is in flight, what happened and why, and what
was learned about the codebase — surviving across sessions.

An agent starting cold reads the board before your first prompt, and files what it finds
without being asked.

## Install

```sh
git clone <this repo> && cd ai-kanban
make install
```

Restart Claude Code. That is the whole setup.

`make install` builds the binary, puts it in `~/.local/bin`, and generates the Claude Code
plugin with that path baked in. It checks the binary runs before writing anything, so a
broken install fails here rather than silently at session start.

### Choosing where things go

```sh
make where                                  # every path it would touch
make install PREFIX=/usr/local              # binary -> /usr/local/bin (may need sudo)
make install CLAUDE_DIR=~/.claude-private   # plugin -> a different Claude config directory
```

`CLAUDE_DIR` is for anyone who keeps Claude Code's configuration somewhere other than
`~/.claude`. It must match the `CLAUDE_CONFIG_DIR` your Claude Code actually uses, or the
plugin lands where nothing looks for it.

Pick a `PREFIX` **your shell already knows about**, so you can also run `ai-kanban backup`
yourself — `make install` warns if the one you chose is not on your `PATH`. The plugin works
either way, because it uses the absolute path rather than searching.

```sh
echo $PATH | tr ':' '\n'     # what yours actually is
```

`/usr/bin` is not an option on macOS — it is protected and unwritable.

### Working on ai-kanban itself

```sh
make install-dev     # plugin only, pointed at ./target/release
```

Then `cargo build --release` and reload, with no reinstall step. Without this a global
install would silently shadow the build you are testing.

### Check it worked

```sh
ai-kanban where     # prints the path to your store
```

The plugin is discovered at session start, so `claude plugin list` shows it from the next
session on.

### Removing it

```sh
make uninstall      # plugin and binary; your board is untouched
```

## Using it

Mostly you don't. The agent calls it — `board` at session start, `task_add` when it spots
something, `note_add` when it works something out, `recall` before debugging something
familiar.

### Workstreams, when one project has several things going on

A long-lived project accumulates work from several directions at once — a feature, an
upgrade, a migration. Left flat, the board fills with tasks from all of them and an agent
starting cold gets told, in detail, about work that is not the work it is doing.

A **workstream** is a named slice of one board. Tell the agent what you are working on and
it narrows:

> we're doing the contact form now

From then on the board shows that slice, other workstreams appear as a one-line summary, and
tasks the agent files join the workstream automatically. It is not a second board: tasks
still block each other across workstreams, nothing has to be merged when one finishes, and
work with no workstream stays visible from everywhere.

For yourself, there is a web UI:

```sh
ai-kanban serve                 # http://127.0.0.1:7373
```

It is a window onto what the agents recorded, for checking status and correcting mistakes —
not a place to run the work from. It shows which workstream the board is in, lets you switch
it, and states which one a new task will join — so nothing lands somewhere you could not see.

### Boards, repos and tracker issues

A board is a project. One appears automatically for any repo an agent works in; for a project
that spans several repositories — a tracker's project, a customer — create one by name and give
it each checkout, or ask the agent to register the repos (`repo_add`):

```sh
ai-kanban board add BMUKN
ai-kanban repo add ~/work/eee-api --board BMUKN
ai-kanban repo add ~/work/eee-web --board BMUKN
```

From then on an agent opening any of them lands on that board, and tickets say which repos they
touch:

```
#4    Fix invoice rounding      (user) [eee-api, eee-web] ref 48213
#9    Contact form spam         (agent) no repo set
```

A ticket can touch several repos, and a repo carries many tickets. An open ticket naming none
shows `no repo set` — a flag, not a block — and `task_show` tells the agent to ask you before
starting. Boards with no repos are unaffected.

A repo can be on several boards — a shared library, say. Its folder still opens on exactly one
of them, its *home*; `ai-kanban repo home <repo> <board>` moves that. A ticket on the wrong
board is moved by the agent (`task_update(move_to: …)`), history and all. `ai-kanban repo
rename|rm|forget` and `ai-kanban board forget` cover the rest; both forgets are a dry run until
you add `--yes`. In the web UI, **All projects**, at the top of the board picker, shows every
board's tickets in one view, and **repos** lists a board's checkouts.

`ref` is the issue in an outside tracker a task mirrors — `48213`, `PROJ-123` — and `recall 48213`
finds it. Which tracker is yours to say, with `AI_KANBAN_REF_URL` below.

## Commands

```sh
ai-kanban projects              # every board in the store
ai-kanban where                 # path to the store
ai-kanban serve [--port N]      # web UI + HTTP API
ai-kanban --help                # all of them
```

### Keep your history

Everything lives in one SQLite file outside version control.

```sh
ai-kanban backup ~/kanban-backup.db
```

Restore by copying that file back over the store.

> Do not just `cp kanban.db`. The store runs in WAL mode, so recent work can still be in a
> `kanban.db-wal` sidecar and a copy of the main file alone silently leaves it behind.
> `backup` goes through SQLite and cannot lose it.

Moving boards between machines, or keeping a readable copy in git:

```sh
ai-kanban export > board.json          # every board
ai-kanban export [PROJECT_NAME] > one.json  # one of them
ai-kanban import board.json            # restores boards that aren't here yet
```

`import` will not merge into a board that already exists — it says so and changes nothing.

### A board that split in two

Two clones of one repo can end up as two half-memories. `projects` shows it as the same
name twice:

```sh
ai-kanban projects
ai-kanban merge "git:github.com/me/repo" "path:/old/checkout"
```

The first board survives and the second is folded into it.

## Configuration

| | |
|---|---|
| `AI_KANBAN_DB` | Use a different store. **Set this for any experiment** — otherwise you are writing to your real memory. |
| `AI_KANBAN_REF_URL` | Where a task's `ref` links to, with `{ref}` as the placeholder — e.g. `https://frs.plan.io/issues/{ref}` or `https://acme.atlassian.net/browse/{ref}`. Set it where you run `ai-kanban serve` and the web UI links every ref to its issue. |
| `CLAUDE_CONFIG_DIR` | Claude Code's own setting. If you use it, pass the same path as `CLAUDE_DIR` to `make install`. |

**macOS and Linux.** Windows is a stated non-goal, not a gap — installation is a Makefile and
the generated hooks are `/bin/sh` one-liners. The `ai-kanban` binary itself builds and runs
there; the install path does not.

## Docs

| Doc | What it settles |
|---|---|
| [`docs/vision.md`](docs/vision.md) | What this is for, and the non-goals |
| [`docs/adoption.md`](docs/adoption.md) | The hooks and the plugin |
| [`docs/tool-design.md`](docs/tool-design.md) | The agent-facing tools |
| [`docs/data-model.md`](docs/data-model.md) | Schema, and why each column exists |
| [`docs/http-api.md`](docs/http-api.md) | The web UI and its API |
| [`docs/architecture.md`](docs/architecture.md) | Layering, durability, dependencies |

Contributing: read [`CLAUDE.md`](CLAUDE.md) first.

## Building from source

```sh
make build      # cargo build --release
make test       # cargo test
```
