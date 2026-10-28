# ai-kanban -- build and install from a checkout.
#
# Most people do not need this: the plugin installs from its marketplace, binary included
# (README, "Install"). This is for building from source and for working on ai-kanban.
#
# Two things get installed:
#
#   1. the binary        -> $(BINDIR), so `ai-kanban` runs in your terminal
#   2. the plugin        -> $(CLAUDE_DIR)/skills/ai-kanban, so Claude Code loads it
#
# The plugin is `plugin/` copied as it is, with the binary placed in its `libexec/`. Its
# configs name the binary through ${CLAUDE_PLUGIN_ROOT}, and plugin/bin/ai-kanban picks the
# build for the machine -- the same layout the release zip has, so what you install from here
# is what users install. See plugin/README.md.

PREFIX     ?= $(HOME)/.local
BINDIR     ?= $(PREFIX)/bin

# Which Claude configuration directory to install the plugin into. Override for a private
# or per-project setup:  make install CLAUDE_DIR=$HOME/.claude-private
# If you keep Claude Code's config somewhere non-default you already set CLAUDE_CONFIG_DIR
# to the same path -- these must agree, or the plugin lands where nothing looks for it.
CLAUDE_DIR ?= $(HOME)/.claude
PLUGIN_DIR  = $(CLAUDE_DIR)/skills/ai-kanban

# A leading `~` in an overridden path is a trap, and this file used to document it as the
# way to override CLAUDE_DIR. Make does not expand `~`, and the recipes quote these paths
# because they can contain spaces -- so `mkdir -p '~/.claude-private/...'` creates a
# directory literally named `~` in the CURRENT directory. The install then reports success
# and lands where nothing looks for it, which is the exact failure the comment above about
# CLAUDE_CONFIG_DIR warns about, arrived at by a different route.
#
# Worse than a wrong install: cleaning up the debris invites `rm -rf ~`, which is one
# tab-completion away from deleting the home directory. Refused here rather than left to
# be discovered.
ifeq ($(patsubst ~%,~,$(CLAUDE_DIR)),~)
$(error CLAUDE_DIR starts with '~', which make does not expand -- it would create a directory named '~' here. Use $$HOME: make install CLAUDE_DIR=$$HOME/.claude-private)
endif
ifeq ($(patsubst ~%,~,$(PREFIX)),~)
$(error PREFIX starts with '~', which make does not expand -- it would create a directory named '~' here. Use $$HOME: make install PREFIX=$$HOME/.local)
endif

BIN         = $(BINDIR)/ai-kanban
BUILT       = target/release/ai-kanban
# The build name plugin/bin/ai-kanban looks for on this machine. The same mapping as in that
# script; tests/plugin_install.rs installs through here and runs the wrapper, so the two
# cannot drift apart unnoticed.
HOST_OS   := $(shell uname -s)-$(shell uname -m)
ifeq ($(HOST_OS),Darwin-arm64)
HOST_BUILD := macos-arm64
else ifneq ($(filter Linux-x86_64 Linux-amd64,$(HOST_OS)),)
HOST_BUILD := linux-x86_64
else ifneq ($(filter Linux-aarch64 Linux-arm64,$(HOST_OS)),)
HOST_BUILD := linux-arm64
endif
# What goes into the plugin's libexec/: the installed binary for `install`, a link to this
# checkout's build for `install-dev`, so a global install cannot shadow the build you test.
PLUGIN_BIN ?= $(BIN)
PLUGIN_LINK ?=

.DEFAULT_GOAL := help
.PHONY: help build test smoke install install-dev plugin uninstall where

help:
	@echo 'ai-kanban'
	@echo
	@echo '  make install       build, install the binary and the plugin'
	@echo '  make install-dev   plugin only, running the build in this checkout (for working on ai-kanban)'
	@echo '  make uninstall     remove the plugin and the installed binary'
	@echo '  make where         show every path this Makefile would touch'
	@echo '  make build/test    cargo build --release / cargo test'
	@echo '  make smoke         two installations sharing a board, end to end'
	@echo
	@echo '  PREFIX=$(PREFIX)'
	@echo '  CLAUDE_DIR=$(CLAUDE_DIR)'
	@echo
	@echo '  Install into a different Claude config directory:'
	@echo '    make install CLAUDE_DIR=$$HOME/.claude-private'

where:
	@echo 'binary      $(BIN)'
	@echo 'plugin      $(PLUGIN_DIR)'
	@echo 'build       ai-kanban-$(HOST_BUILD), from $(PLUGIN_BIN)'
	@echo 'store       '`$(BUILT) where 2>/dev/null || echo '(build first)'`

build:
	cargo build --release

test:
	cargo test --workspace

# Two installations (test-a.db, test-b.db) sharing a board through a relay, driven through the
# real binary's CLI and MCP: tests/e2e_two_installs.rs.
smoke:
	cargo test --test e2e_two_installs -- --nocapture

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
	  echo '        terminal. The plugin still works -- it carries its own copy.' ;; \
	esac
	@echo
	@echo 'Restart Claude Code to pick it up.'

# The plugin without touching the binary, pointed at this checkout. What you want while
# working on ai-kanban: `cargo build --release` then reload, with no reinstall step.
install-dev: build
	@$(MAKE) --no-print-directory plugin PLUGIN_BIN='$(CURDIR)/$(BUILT)' PLUGIN_LINK=1
#	The same link inside the checkout, for `claude --plugin-dir ./plugin`.
	@mkdir -p plugin/libexec
	@ln -sfn '$(CURDIR)/$(BUILT)' 'plugin/libexec/ai-kanban-$(HOST_BUILD)'
	@echo 'Plugin installed at $(PLUGIN_DIR), running $(CURDIR)/$(BUILT)'
	@echo 'Note: a copy installed from the marketplace takes precedence over this one.'

# Copy the plugin and put the binary where its wrapper looks. Separated from `install` so
# `install-dev` can reuse it with a link instead of a copy.
plugin:
	@test -n '$(HOST_BUILD)' || { echo "make: no ai-kanban build for $$(uname -s) $$(uname -m)" >&2; exit 1; }
	@rm -rf '$(PLUGIN_DIR)'
	@mkdir -p '$(PLUGIN_DIR)/libexec'
	@cp -R plugin/.claude-plugin plugin/hooks plugin/bin plugin/.mcp.json '$(PLUGIN_DIR)/'
	@if [ -n '$(PLUGIN_LINK)' ]; then \
	  ln -s '$(PLUGIN_BIN)' '$(PLUGIN_DIR)/libexec/ai-kanban-$(HOST_BUILD)'; \
	else \
	  install -m 755 '$(PLUGIN_BIN)' '$(PLUGIN_DIR)/libexec/ai-kanban-$(HOST_BUILD)'; \
	fi

uninstall:
	rm -rf '$(PLUGIN_DIR)'
	rm -f '$(BIN)'
	@echo 'Removed. Your board is untouched -- it lives in the store, not here.'
	@echo 'Run `ai-kanban where` before uninstalling if you want the path.'
