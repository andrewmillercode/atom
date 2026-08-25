# Makefile for the single `atom` executable
#
# Dev install (symlinks to debug build):
#   make install-dev
#   make install-dev PREFIX=~/.local   # default
#
# Release install (copies release build):
#   make install
#   sudo make install PREFIX=/opt/homebrew
#   sudo make install PREFIX=/usr/local
#
# Uninstall (removes everything):
#   make uninstall
#   make uninstall PREFIX=/opt/homebrew
#
# Uninstall stops the background session server, deletes the binary, wipes
# atom's config and data dirs (config.json, skills, sessions, credentials,
# logs), removes the PATH line install.sh added to your shell rc, and
# removes BIN_DIR if it ends up empty.

PREFIX ?= $(HOME)/.local
BIN_DIR = $(PREFIX)/bin
# Match the Rust code: XDG_DATA_HOME/XDG_CONFIG_HOME honored, else ~/.local/share and ~/.config.
DATA_DIR = $(shell sh -c 'printf "%s" "$${XDG_DATA_HOME:-$$HOME/.local/share}/atom"')
CONFIG_DIR = $(shell sh -c 'printf "%s" "$${XDG_CONFIG_HOME:-$$HOME/.config}/atom"')
# BIN_DIR with slashes escaped for use inside a sed address.
BIN_DIR_SED = $(subst /,\/,$(BIN_DIR))

CARGO ?= cargo
CARGO_BUILD_FLAGS ?=

.PHONY: all build build-release install-dev install uninstall clean

all: build

build:
	$(CARGO) build $(CARGO_BUILD_FLAGS) --bin atom

build-release:
	$(CARGO) build --release --bin atom

install-dev: build
	install -d $(BIN_DIR)
	ln -sf $(CURDIR)/target/debug/atom $(BIN_DIR)/atom

install: build-release
	install -d $(BIN_DIR)
	install -m 755 $(CURDIR)/target/release/atom $(BIN_DIR)/atom

uninstall:
	@echo "==> stopping background session server"
	@if [ -f "$(DATA_DIR)/server.pid" ]; then \
		kill "$$(cat "$(DATA_DIR)/server.pid")" 2>/dev/null || true; \
		sleep 1; \
	fi
	@echo "==> removing $(BIN_DIR)/atom"
	rm -f $(BIN_DIR)/atom
	@echo "==> removing config $(CONFIG_DIR)"
	@echo "    and data $(DATA_DIR) (sessions, credentials, logs)"
	rm -rf $(CONFIG_DIR) $(DATA_DIR)
	@echo "==> removing PATH line added by install.sh (exact match only)"
	@for rc in "$$HOME/.zshrc" "$$HOME/.bashrc" "$$HOME/.profile"; do \
		if [ -f "$$rc" ]; then \
			sed -i.bak '/^export PATH="$(BIN_DIR_SED):$$PATH"$$/d' "$$rc" && rm -f "$$rc.bak"; \
		fi \
	done
	@rmdir $(BIN_DIR) 2>/dev/null || true
	@echo "==> atom uninstalled"

clean:
	$(CARGO) clean

# Tag, build, and attach the binary to a GitHub release (requires gh authed).
#   make release VERSION=v0.1.0
release:
	@test -n "$(VERSION)" || (echo "usage: make release VERSION=v0.1.0" && exit 1)
	./scripts/release.sh "$(VERSION)"
