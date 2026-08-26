# AGENTS.md

Guidelines for agents working on the atom codebase.

## Repository structure

This is the Rust rewrite of atom. It is a Cargo workspace under `crates/`:

- `crates/atom` — the `atom` TUI client and `atoms` background server binaries.
- `crates/atom-server` — background session-server library used by `atom`.
- `crates/atom-tui` — TUI implementation.
- `crates/atom-core` — shared types and helpers.
- `crates/atom-tools`, `crates/atom-sandbox` — tool execution and sandboxing.

The repo no longer tracks a prebuilt binary at the root. Source changes are
validated with `cargo check` / `cargo test`, not a checked-in executable.

## Build and dev install

To build the executable:

```sh
cargo build --bin atom
```

To make `atom` available on your PATH during development:

```sh
make install-dev
```

This symlinks `target/debug/atom` and `target/debug/atoms` into
`~/.local/bin`. Make sure `~/.local/bin` is on your `PATH`. The client
starts the `atoms` server automatically if it isn't already running.

## Tests

Run the test suite after any change:

```sh
cargo test
```

For a faster subset focused on the client/server crates:

```sh
cargo test -p atom -p atom-server -p atom-tui
```

## Formatting and linting

Keep new code `rustfmt`-clean:

```sh
cargo fmt --check
cargo clippy --workspace
```

Do not reformat unrelated files in the same commit; keep diffs minimal.

## Binary name

There are two binaries in `crates/atom/Cargo.toml`:

- **`atom`** — the TUI client. Automatically starts `atoms` if no server is
  running.
- **`atoms`** — the background session server. Cannot be launched directly;
  it requires a launch token set by `atom` (`_ATOM_LAUNCH=managed`).

Both are built by `cargo build` and installed together by `make install-dev`
or `make install`.
