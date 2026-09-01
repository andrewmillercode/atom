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
cargo build
```

To make `atom` available on your PATH during development:

```sh
make dev
```

This symlinks the debug build into `~/.local/bin` as `atomdev` and
`atomsdev` (plain symlinks to `target/debug/atom` and
`target/debug/atoms` — there are no separate dev binaries; release
installs keep the `atom`/`atoms` names). Make sure `~/.local/bin` is on
your `PATH`. The client starts the `atomsdev` server automatically if
it isn't already running.

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
- **`atomdev` / `atomsdev`** — dev aliases: plain symlinks to the debug
  `atom`/`atoms` artifacts, created by `make dev` (see
  `crates/atom/Cargo.toml`). Meaningful only in the debug profile; the
  dev/release flavor is keyed on `cfg!(debug_assertions)`, not the name.

Two binaries are built by `cargo build` and installed by `make dev`
(as `atomdev`/`atomsdev` symlinks) or `make install` (as
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

## Build performance

Measured on the reference M4 Pro (12 cores), 2026-09-01, after the
profile/dependency changes below. Keep these numbers roughly true when
touching the build config; re-measure if you add a heavy dependency.

| Workflow                     | Time  | Notes                                   |
|------------------------------|-------|-----------------------------------------|
| Fresh full `cargo build`     | ~18s  | 415-crate graph, all 12 cores           |
| Content edit → rebuild       | ~2s   | warm incremental (edit in any crate)    |
| Link either bin              | ~0.5s | was 8s before dep-debuginfo was dropped |
| `cargo test --no-run`        | ~11s  | 11 test binaries                        |
| `cargo clippy` fresh         | ~8s   | check-only, no codegen                  |
| `cargo fmt --check`          | 0.3s  |                                         |
| `cargo build --release`      | ~49s  | thin LTO + 1 CGU, leave as is           |

What makes this possible (do not casually revert):

- `[profile.dev.package."*"] debug = false` in the workspace Cargo.toml:
  dependencies carry no debuginfo; the six workspace crates keep
  line tables via explicit overrides. This is what collapsed link time
  (8.2s → 0.5s per binary) and the test build (53s → 11s). Dep rlibs
  went from ~2.0GB to ~0.9GB per fresh build.
- syntect uses `regex-fancy` (pure Rust), not `regex-onig` — no Oniguruma
  C compile. Same API surface for what `render/highlight.rs` needs.
- image codecs are limited to the two actually used: `png`, `jpeg`.
  Re-add a codec only when code needs it.
- Workspace crates form a serial chain core → sandbox → tools → server
  → tui → bins. A public-API change in atom-core recompiles the whole
  chain; internal changes ride the incremental green path (~2s). Keep
  new code in the deepest crate that needs it, not in atom-core by
  default.

Operational notes:

- Never run two cargo invocations on the same target dir concurrently
  (they serialize on the build lock and look like multi-minute hangs).
  Use `cargo check` for quick validation; it shares the lock but not
  codegen. rust-analyzer should use a separate target dir
  (`rust-analyzer.cargo.targetDir = true`) to stay out of the way.
- If builds feel uniformly slow (even warm ones), check
  `ls target/debug/deps | wc -l`. A healthy dir has a few thousand
  files; tens of thousands means stale accumulation — `cargo clean`
  (a fresh rebuild is only ~18s) after rustc updates.
- macOS: running many freshly compiled build scripts is slow if the
  terminal app is not registered as a Developer Tool
  (System Settings → Privacy & Security → Developer Tools → add your
  terminal). This alone can add minutes to fresh builds.
- Nightly rustc's parallel frontend (`-Zthreads=8`) was measured at
  ~10% on fresh builds and slightly slower on warm rebuilds — not
  worth an extra toolchain; don't re-add it expecting more.
