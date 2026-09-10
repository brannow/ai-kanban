# Plugin templates

These are the **source** for the Claude Code plugin. `make install` renders them into
`$(CLAUDE_DIR)/skills/ai-kanban/`, substituting `@BIN@` with the absolute path of the
binary it just installed.

## Why generated rather than checked in

A checked-in plugin config cannot know where the binary lives, because a plugin is
installed by being *copied* somewhere. The previous version solved that with a 60-line
`bin/ai-kanban` shell script that searched six locations at session start, plus an
`AI_KANBAN_BIN` escape hatch — reconstructing at runtime something the installer already
knew. It also shipped broken for four days, pointing at a path that only resolved inside an
ai-kanban checkout, and nothing caught it (see note #15 on the board).

Baking the path in at install time removes the search, the script and the escape hatch. The
failure moves from *silent, at session start* to *loud, at install time*, where someone is
watching — `make install` verifies the binary runs before it writes anything.

## The one thing the templates still have to protect

`hooks.json` guards each command with `[ -x "@BIN@" ] && … || true`. Hook code must never
fail loudly (`CLAUDE.md`): a hook that complains is a hook the user deletes, and deleting it
takes the bundled MCP server with it. So if the binary is later moved or removed, the hooks
go quiet rather than erroring on every session.

`mcp.json` deliberately does **not** guard: an MCP server that dies silently is a board
that is mysteriously absent, which is harder to diagnose than a startup error.

## Keep the top level of `hooks.json` to what the loader allows

The plugin loader accepts exactly four top-level keys — `description`, `hooks`, `modules`,
`surface` — and warns on anything else: `ai-kanban: hooks.json: unknown key "x" ignored`, on
every session start. That is the same "hook that complains" failure as above, arriving by a
different route: the hooks still run, but the plugin is visibly noisy, and noisy is what gets
uninstalled.

This cost us a real warning. The templates carried a `"_comment"` key explaining that the
rendered file is generated — helpful to a contributor, but `hooks.json` ships to *consumers*,
and a note about a Makefile they do not have never belonged there. `description` is the
consumer-facing field: it says what the hooks do, nothing about how this repo builds them.

`claude plugin validate` does **not** catch this — it validates the manifest, not the hook
file's top-level keys. The guard is `tests/plugin_install.rs`, which asserts the rendered
top level against the allowlist.

The rendered `hooks.json` and `mcp.json` are generated on every `make install`. Editing the
installed copy is pointless; edit the `.in` templates here.
