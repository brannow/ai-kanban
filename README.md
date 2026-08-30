# ai-kanban

A kanban board an agent uses as memory. What is in flight, what happened and why, and what
was learned about the codebase — surviving across sessions.

An agent starting cold reads the board before your first prompt, and files what it finds
without being asked.

## Install

Two pieces: a binary, and a plugin that points Claude Code at it.

```sh
git clone <this repo> && cd ai-kanban
cargo build --release
install -m 755 target/release/ai-kanban ~/.local/bin/    # no sudo, already on most PATHs
cp -R .claude/skills/ai-kanban ~/.claude/skills/
```

Restart Claude Code. That is the whole setup.

### Putting the binary somewhere else

Any directory works — the plugin searches `~/.local/bin`, `~/.cargo/bin`,
`/opt/homebrew/bin`, `/usr/local/bin` and then your `PATH`. Pick one **your shell already
knows about**, so you can run `ai-kanban backup` yourself:

```sh
echo $PATH | tr ':' '\n'                                  # what yours actually is
```

```sh
install -m 755 target/release/ai-kanban ~/.local/bin/     # no sudo
sudo install -m 755 target/release/ai-kanban /usr/local/bin/   # traditional, needs sudo
cargo install --path .                                    # -> ~/.cargo/bin
```

`cargo install` is the odd one out: the plugin finds it there, but `~/.cargo/bin` is often
**not** on `PATH` (it isn't by default on macOS with Homebrew Rust), so the commands below
won't work in your terminal. Use it only if that directory is already on yours.

`/usr/bin` is not an option on macOS — it is protected and unwritable.

### Per-project instead of everywhere

Drop the plugin in one repo rather than all of them:

```sh
mkdir -p /path/to/repo/.claude/skills
cp -R .claude/skills/ai-kanban /path/to/repo/.claude/skills/
```

Claude Code asks you to trust the workspace the first time. The personal install above does
not ask.

### Check it worked

```sh
ai-kanban where     # prints the path to your store
```

The plugin is discovered at session start, so `claude plugin list` shows
`ai-kanban@skills-dir` from the next session on. Turn it off with
`claude plugin disable ai-kanban@skills-dir`.

## Using it

Mostly you don't. The agent calls it — `board` at session start, `task_add` when it spots
something, `note_add` when it works something out, `recall` before debugging something
familiar.

For yourself, there is a web UI:

```sh
ai-kanban serve                 # http://127.0.0.1:7373
```

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
| `AI_KANBAN_BIN` | Point the plugin at a specific binary. Searched before everything else. |

**macOS and Linux.** Windows is a stated non-goal, not a gap — the plugin's entry point is a
`#!/bin/sh` script. The `ai-kanban` binary itself builds and runs there; the plugin does not.

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
cargo build --release
cargo test
```
