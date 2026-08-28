# Makefile for the `atom` / `atoms` executables
#
# Dev install (debug build, emitted by cargo as real atomdev/atomsdev
# binaries and linked into ~/.local/bin):
#   make dev
#   make dev PREFIX=~/.local   # default
#
# Dev installs coexist with release installs: different binary names
# (atomdev/atomsdev vs atom/atoms) and a separate atom-dev data/config
# dir (see crates/atom-core/src/build.rs). Dev builds never auto-update.
#
# Release install (copies release build):
#   make install
#   sudo make install PREFIX=/opt/homebrew
#   sudo make install PREFIX=/usr/local
#
# Uninstall (removes everything — dev and release):
#   make uninstall
#   make uninstall PREFIX=/opt/homebrew
#
# Uninstall stops the background session servers, deletes the binaries,
# wipes atom's config and data dirs (config.json, skills, sessions,
# credentials, logs), removes the PATH line install.sh added to your
# shell rc, and removes BIN_DIR if it ends up empty.

PREFIX ?= $(HOME)/.local
BIN_DIR = $(PREFIX)/bin
# Match the Rust code: XDG_DATA_HOME/XDG_CONFIG_HOME honored, else ~/.local/share and ~/.config.
DATA_DIR = $(shell sh -c 'printf "%s" "$${XDG_DATA_HOME:-$$HOME/.local/share}/atom"')
CONFIG_DIR = $(shell sh -c 'printf "%s" "$${XDG_CONFIG_HOME:-$$HOME/.config}/atom"')
# BIN_DIR with slashes escaped for use inside a sed address.
BIN_DIR_SED = $(subst /,\/,$(BIN_DIR))

CARGO ?= cargo
CARGO_BUILD_FLAGS ?=

.PHONY: all dev build build-release install uninstall clean release

all: build

# Dev build (debug profile; also emits the atomdev/atomsdev dev aliases),
# then link atomdev/atomsdev into $(BIN_DIR) so they're callable.
dev: build
	install -d $(BIN_DIR)
	ln -sf $(CURDIR)/target/debug/atomdev $(BIN_DIR)/atomdev
	ln -sf $(CURDIR)/target/debug/atomsdev $(BIN_DIR)/atomsdev

build:
	$(CARGO) build $(CARGO_BUILD_FLAGS) --bin atom --bin atoms --bin atomdev --bin atomsdev

build-release:
	$(CARGO) build --release --bin atom --bin atoms

install: build-release
	install -d $(BIN_DIR)
	install -m 755 $(CURDIR)/target/release/atom $(BIN_DIR)/atom
	install -m 755 $(CURDIR)/target/release/atoms $(BIN_DIR)/atoms

uninstall:
	@echo "==> stopping background session servers (release and dev)"
	@for d in "$(DATA_DIR)" "$(DATA_DIR)-dev"; do \
		if [ -f "$$d/server.pid" ]; then \
			kill "$$(cat "$$d/server.pid")" 2>/dev/null || true; \
		fi \
	done
	sleep 1
	@echo "==> removing binaries from $(BIN_DIR)"
	rm -f $(BIN_DIR)/atom $(BIN_DIR)/atoms $(BIN_DIR)/atomdev $(BIN_DIR)/atomsdev
	@echo "==> removing config $(CONFIG_DIR)"
	@echo "    and data $(DATA_DIR) (sessions, credentials, logs)"
	@echo "    plus the -dev dirs used by atomdev/atomsdev"
	rm -rf $(CONFIG_DIR) $(CONFIG_DIR)-dev $(DATA_DIR) $(DATA_DIR)-dev
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

# Interactive release: confirm version, build, pick notes, tag a commit,
# publish the GitHub release with the binary attached (requires gh authed).
#   make release
release:
	@./scripts/release.sh
