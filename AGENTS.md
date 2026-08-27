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
cargo build --bin atom --bin atoms --bin atomdev --bin atomsdev
```

To make `atom` available on your PATH during development:

```sh
make install-dev
```

This links the debug build into `~/.local/bin` as `atomdev` and
`atomsdev` — `cargo build` emits real `atomdev`/`atomsdev` binaries
(release installs keep the `atom`/`atoms` names). Make sure
`~/.local/bin` is on your `PATH`. The client starts the `atomsdev`
server automatically if it isn't already running.

Dev and release never mix: dev binaries and the auto-updater are
gated on the debug build, and dev state lives in `atom-dev` data/
config dirs (see `crates/atom-core/src/build.rs`).

## Dependencies

Registry dependencies come from crates.io, pinned by the committed
`Cargo.lock`. Published crate versions are immutable on crates.io and
tarballs are checksum-verified against the lockfile at download, so the
lockfile alone gives reproducible builds. `vendor/` is not used and is
gitignored — do not commit it.

- To add or bump a dependency use `cargo add` / `cargo update` normally,
  then commit the updated `Cargo.lock`.
- `ratatex` (LaTeX/Kitty math rendering in `atom-tui`) is pinned with
  `=0.1.0` and `default-features = false`; the `ratex-*` engine crates sit
  at `0.1.14` in `Cargo.lock`. Those exact versions passed the 2026-08
  supply-chain audit — do not bump them casually.
- If an offline/self-contained build artifact is ever needed, run
  `cargo vendor` into a scratch directory and do not commit the result.

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

There are two binaries in `crates/atom/Cargo.toml`, plus dev aliases:

- **`atom`** — the TUI client. Automatically starts `atoms` if no server is
  running.
- **`atoms`** — the background session server. Cannot be launched directly;
  it requires a launch token set by `atom` (`_ATOM_LAUNCH=managed`).
- **`atomdev` / `atomsdev`** — dev aliases built from the same sources as
  `atom`/`atoms` (see `crates/atom/Cargo.toml`). Real binaries, not
  symlinks; meaningful only in the debug profile.

All four are built by `cargo build` and installed together by
`make install-dev` (as `atomdev`/`atomsdev`) or `make install` (as
`atom`/`atoms`).

## Bundled instructions

`instructions/system-prompt.md` is the bundled system prompt agents see
when they run inside atom. AGENTS.md files (this one included) are merged
separately as extra project context; they never replace the bundled
system prompt.

There is no separate tools doc: tool documentation lives in the tool
definitions themselves. When you add a tool, or change one in a
substantial way (new parameters, changed semantics, new workflow
guidance), update its definition in `crates/atom-tools/src/defs.rs` in
the same change so the model-facing instructions stay accurate.
Behavior guidance that is not tool-specific belongs in
`instructions/system-prompt.md`.
