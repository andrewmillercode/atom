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
# Uninstall:
#   make uninstall
#   make uninstall PREFIX=/opt/homebrew

PREFIX ?= $(HOME)/.local
BIN_DIR = $(PREFIX)/bin

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
	rm -f $(BIN_DIR)/atom

clean:
	$(CARGO) clean

# Tag, build, and attach the binary to a GitHub release (requires gh authed).
#   make release VERSION=v0.1.0
release:
	@test -n "$(VERSION)" || (echo "usage: make release VERSION=v0.1.0" && exit 1)
	./scripts/release.sh "$(VERSION)"
