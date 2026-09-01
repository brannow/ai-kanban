# ai-kanban -- build and install.
#
# Two things get installed, and they are separate on purpose:
#
#   1. the binary        -> $(BINDIR), somewhere your shell can find it
#   2. the plugin config -> $(CLAUDE_DIR)/skills/ai-kanban, so Claude Code loads it
#
# The plugin config is GENERATED, not copied, with the binary's absolute path baked in.
# See plugin/README.md for why -- briefly: a copied config cannot know where the binary
# went, and the previous answer was a 60-line shell script that searched six directories at
# session start to rediscover what the installer already knew.

PREFIX     ?= $(HOME)/.local
BINDIR     ?= $(PREFIX)/bin

# Which Claude configuration directory to install the plugin into. Override for a private
# or per-project setup:  make install CLAUDE_DIR=~/.claude-private
# If you keep Claude Code's config somewhere non-default you already set CLAUDE_CONFIG_DIR
# to the same path -- these must agree, or the plugin lands where nothing looks for it.
CLAUDE_DIR ?= $(HOME)/.claude
PLUGIN_DIR  = $(CLAUDE_DIR)/skills/ai-kanban

BIN         = $(BINDIR)/ai-kanban
BUILT       = target/release/ai-kanban
# The path baked into the generated config. `install` points it at the installed copy;
# `install-dev` overrides it to the checkout, so a global install cannot shadow the build
# you are testing.
PLUGIN_BIN ?= $(BIN)

.DEFAULT_GOAL := help
.PHONY: help build test install install-dev plugin uninstall where

help:
	@echo 'ai-kanban'
	@echo
	@echo '  make install       build, install the binary, generate the plugin'
	@echo '  make install-dev   plugin only, pointed at this checkout (for working on ai-kanban)'
	@echo '  make uninstall     remove the plugin and the installed binary'
	@echo '  make where         show every path this Makefile would touch'
	@echo '  make build/test    cargo build --release / cargo test'
	@echo
	@echo '  PREFIX=$(PREFIX)'
	@echo '  CLAUDE_DIR=$(CLAUDE_DIR)'
	@echo
	@echo '  Install into a different Claude config directory:'
	@echo '    make install CLAUDE_DIR=$$HOME/.claude-private'

where:
	@echo 'binary      $(BIN)'
	@echo 'plugin      $(PLUGIN_DIR)'
	@echo 'baked path  $(PLUGIN_BIN)'
	@echo 'store       '`$(BUILT) where 2>/dev/null || echo '(build first)'`

build:
	cargo build --release

test:
	cargo test

install: build
	@mkdir -p '$(BINDIR)'
	install -m 755 '$(BUILT)' '$(BIN)'
#	Verify before generating anything. This is the whole point of installing rather than
#	searching: the binary is proven to run HERE, where someone is watching, instead of
#	failing silently inside a session hours later.
	@'$(BIN)' where >/dev/null || { echo 'make: installed binary does not run: $(BIN)' >&2; exit 1; }
	@$(MAKE) --no-print-directory plugin PLUGIN_BIN='$(BIN)'
	@echo
	@echo 'Installed.'
	@echo '  binary  $(BIN)'
	@echo '  plugin  $(PLUGIN_DIR)'
	@case ':$(PATH):' in *':$(BINDIR):'*) ;; *) \
	  echo; \
	  echo "  NOTE: $(BINDIR) is not on your PATH, so \`ai-kanban\` will not run in your"; \
	  echo '        terminal. The plugin still works -- it uses the absolute path above.' ;; \
	esac
	@echo
	@echo 'Restart Claude Code to pick it up.'

# The plugin without touching the binary, pointed at this checkout. What you want while
# working on ai-kanban: `cargo build --release` then reload, with no reinstall step.
install-dev: build
	@$(MAKE) --no-print-directory plugin PLUGIN_BIN='$(CURDIR)/$(BUILT)'
	@echo 'Plugin installed at $(PLUGIN_DIR), pointed at $(CURDIR)/$(BUILT)'

# Render the templates. Separated from `install` so `install-dev` can reuse it with a
# different PLUGIN_BIN rather than duplicating the substitution.
plugin:
	@mkdir -p '$(PLUGIN_DIR)/.claude-plugin'
	@cp plugin/plugin.json '$(PLUGIN_DIR)/.claude-plugin/plugin.json'
#	`|` as the sed delimiter: the substitution is a filesystem path and will contain slashes.
	@sed 's|@BIN@|$(PLUGIN_BIN)|g' plugin/hooks.json.in > '$(PLUGIN_DIR)/hooks.json'
	@sed 's|@BIN@|$(PLUGIN_BIN)|g' plugin/mcp.json.in   > '$(PLUGIN_DIR)/mcp.json'

uninstall:
	rm -rf '$(PLUGIN_DIR)'
	rm -f '$(BIN)'
	@echo 'Removed. Your board is untouched -- it lives in the store, not here.'
	@echo 'Run `ai-kanban where` before uninstalling if you want the path.'
