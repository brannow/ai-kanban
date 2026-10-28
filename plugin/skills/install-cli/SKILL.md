---
name: install-cli
description: Link the ai-kanban command into ~/.local/bin so it also runs in the user's own terminal. Only when the user asks for it.
disable-model-invocation: true
---

Run exactly this in the shell:

```sh
ai-kanban install-cli --link "${CLAUDE_PLUGIN_DATA}/bin/ai-kanban"
```

Then tell the user what it printed:

- **Linked, or already linked:** done. The link follows plugin updates by itself.
- **Something is already there:** say what it found, and ask the user before running it again
  with `--replace` added. It may be a binary they put there on purpose.
- **Does not exist yet:** the plugin creates its link when a session starts; ask the user to
  start a new Claude Code session and try again.
- **Not on your PATH:** pass on the `export PATH=...` line it printed, for their shell profile.
  Do not edit the profile yourself.
