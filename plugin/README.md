# ai-kanban plugin

A kanban board an agent uses as persistent memory: what is in flight, what happened and why,
and what was learned about the codebase -- surviving across sessions.

Install it in Claude Code:

```
/plugin install ai-kanban --marketplace brannow/ai-kanban
```

Documentation: https://github.com/brannow/ai-kanban

## What is in here

| Path | What it is |
|---|---|
| `.claude-plugin/plugin.json` | The manifest. No `version`: the release zip's digest decides when an update is new. |
| `hooks/hooks.json` | The session-start briefing and the notes for files a tool touches. |
| `.mcp.json` | The board's MCP server. |
| `bin/ai-kanban` | Runs the build for this machine from `libexec/`. |
| `libexec/ai-kanban-<os>-<arch>` | The binaries: in the release zip, or put there by `make install` / `make install-dev`. Not in git. |

This directory is the plugin as it ships: the release zip, `make install` and `make
install-dev` all use it unchanged. Why the binary is found this way, and the three designs
that came before, is in `docs/adoption.md` under "How the plugin finds its binary". Read it
before changing the wrapper; the short version is that it picks between builds, never
between places, and that difference is the whole lesson of note #15.

Two rules the files here must keep, both asserted by `tests/plugin_install.rs`:

- **Hooks never fail loudly.** They run in every directory Claude Code is opened in; a hook
  that complains is a hook the user deletes, taking the MCP server with it. Hence
  `2>/dev/null || true`. The MCP server is deliberately not silenced.
- **`hooks/hooks.json` keeps to the loader's four top-level keys** (`description`, `hooks`,
  `modules`, `surface`). Anything else warns on every session start, and `claude plugin
  validate` does not catch it.
