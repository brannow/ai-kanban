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

## No comments in the templates

JSON has no comments, and the obvious workaround — a `"_comment"` key — is not harmless
here: Claude Code validates `hooks.json` and prints `unknown key "_comment" ignored` at
every session start. That is the loud hook this whole file exists to prevent, so the
explanation lives in this README instead.
