# atom

A Rust TUI chat client for Ollama, OpenCode Go, and any OpenAI-compatible endpoint. Built with [ratatui](https://github.com/ratatui-org/ratatui). It uses a central session server so conversations persist across invocations and can be resumed from any directory.

It works the way opencode2's Ollama provider does: it POSTs to `/v1/chat/completions` and parses the streaming SSE chunks `{"choices":[{"delta":{"content":...}}]}` as they arrive.

## Architecture

Atom has two parts: a **client** (the `atom` command you run) and a **session server** (a background process the client manages automatically).

- When you run `atom`, it checks if a server is already running on its Unix socket (`~/.local/share/atom/atom.sock`). If so, it connects. If not, it starts one as a detached background process and waits for it to be ready.
- The server holds sessions in memory and persists each one as a JSON file in `~/.local/share/atom/sessions/`. Sessions survive server restarts — the server loads them from disk on startup.
- Only one server runs at a time. If a second instance tries to start, it detects the existing one and exits cleanly.
- The client is a ratatui TUI that handles terminal I/O, the conversation view, slash commands, and overlay selectors for models and sessions. The server handles model API calls, tool execution, and session persistence.
- **Vector Search** (`vector_search`) runs [Semble](https://github.com/MinishLab/semble) `search` at a pinned version (`semble==0.5.5`) via `uvx`. It does not expose `find_related`. Requires [uv](https://docs.astral.sh/uv/).
- **Grep** and **Glob** call [ripgrep](https://github.com/BurntSushi/ripgrep) (`rg`): literal `-F` for content search, `rg --files` for globs. Requires `rg` on `PATH`.
- **Skills** are Cursor-style `SKILL.md` files. Atom lists their names and descriptions in the system prompt and loads a full skill when the model calls the `skill` tool.
- **MCP** servers from `mcp.json` (stdio or streamable HTTP) are exposed as extra tools named `mcp_<server>_<tool>`.

## Setup

No key needed if you have the Ollama app installed and signed in (run `ollama signin` once). The CLI will talk to the cloud through your local Ollama at `http://localhost:11434/v1`, exactly like opencode2 does.

To talk to ollama.com directly instead, get an API key at <https://ollama.com/settings/keys> and export it:

```sh
export OLLAMA_API_KEY=ollama_...
```

To use OpenCode Go models, get an API key from the OpenCode Zen console and export it:

```sh
export OPENCODE_GO_API_KEY=sk-...
```

You can also save keys to the providers directory instead of using env vars:

```
~/.local/share/atom/providers/ollama-cloud   # ollama.com API key
~/.local/share/atom/providers/opencode-go    # OpenCode Go API key
```

## Usage

The TUI has a scrolling conversation view with a fixed input line at the bottom and a status bar showing the current model, thinking level, token usage, and status. The token usage indicator shows input, output, cache hit rate, and context fill, e.g. `Input 7.6K, Output 800, Cache 74%, Context 6% (8.4K/128K)`. Input, output, and context are the latest model round; cache hit rate is a token-weighted average across every round in the session. Fields the provider didn't report render as `--`. Counts come from streamed chunks (`stream_options.include_usage`) and are persisted with the session.

```sh
atom
> what is a monad?
> /model    # switch models (overlay selector with search)
> /new      # start a new session
> /sessions # pick a session (overlay selector with search)
> /thinking # toggle expanded thinking blocks
> /quit     # or Ctrl-C / Ctrl-D
```

**Tab** and **Ctrl+T** cycle the current model's reasoning levels (from models.dev). Slash commands are typed directly and submitted with Enter.

**Shift+Enter** (in terminals with kitty keyboard protocol support: kitty, Ghostty, iTerm2, Alacritty, foot, WezTerm, ...), **Alt+Enter**, and **Ctrl+J** insert a newline in the prompt instead of sending it.

### Model selector

Running `atom` without `-model` opens an interactive model selector on startup. It fetches available models from every configured provider (Ollama Cloud, OpenCode Go, and local Ollama) and lets you search and pick:

- **Type** to filter by model name or provider.
- **↑↓** to navigate the list.
- **Enter** to select a model and start chatting.
- **Esc** to cancel.

You can also switch models mid-session with `/model`, which opens the same selector. Selecting a model switches the current session to it: the conversation history stays, and subsequent messages are answered by the new model.

### Commands

- `/model` — open the model selector to switch the current session's model (conversation stays).
- `/settings` — choose the compaction model and the provider behind the stable `web_search` tool.
- `/new` — create a new session and switch to it.
- `/sessions` — list all sessions, marking the active one with `→`. Ctrl+D deletes the highlighted session.
- `/mcps` — list enabled MCP servers (same footer menu as subagents).
- `/skills` — list available skills (same footer menu as subagents).
- `/stats` — show token usage for all time. `/stats 30` shows the last 30 days (slash menu offers it after typing `/st`).
- `/thinking` — toggle expanded thinking blocks: off collapses each reasoning block to `Thinking` plus a muted spinner (in progress) or `Thinking (8.3s)` (finished). The slash-menu description shows `(on)` or `(off)`.
- `/quit` — exit the client (the server keeps running).

### Resuming a session

```sh
atom -session a2274dd30fdd0c52
```

### Options

```sh
atom                          # open model selector
atom -model qwen3.5:cloud     # start with a specific model
atom -url http://localhost:11434/v1 -model deepseek-v4-flash:cloud
```

- `-model` — model to chat with. Omit it to open the interactive model selector, which fetches models from all configured providers.
- `-key` — API key, default `$OLLAMA_API_KEY` or `$OPENCODE_GO_API_KEY` (depending on the provider).
- `-url` — base URL. When omitted, atom auto-detects the provider from the model or lets you pick via the selector.
- `-session` — resume an existing session by ID.
- `-serve` — run the session server directly (the client does this automatically when needed).

## Providers

Atom supports three providers, auto-detected from available credentials:

| Provider | Base URL | Key env var | Key file |
|---|---|---|---|
| Ollama Cloud | `https://ollama.com/v1` | `OLLAMA_API_KEY` | `~/.local/share/atom/providers/ollama-cloud` |
| OpenCode Go | `https://opencode.ai/zen/go/v1` | `OPENCODE_GO_API_KEY` | `~/.local/share/atom/providers/opencode-go` |
| Ollama Local | `http://localhost:11434/v1` | (none needed) | — |

When you run `atom` without `-model`, it fetches the model list from every provider whose credentials are available and merges them into a single searchable list. Selecting a model automatically routes chat requests to the right provider.

## Settings and web search

On first run, Atom opens a skippable settings overlay after chat-model setup. Open it later with `/settings`. Preferences are stored in `$XDG_CONFIG_HOME/atom/config.json` (default `~/.config/atom/config.json`):

```json
{
  "version": 1,
  "compaction": {
    "provider": "ollama-local",
    "model": "deepseek-v4-flash:0731"
  },
  "web_search": {
    "server": "parallel",
    "tool": "web_search"
  }
}
```

The compaction selection is used for automatic, manual, and mid-turn compaction; title generation reuses it. The model always sees one built-in capability named `web_search`. Atom routes that capability to the selected backend:

- **Parallel** (default): anonymous; `PARALLEL_API_KEY` is optional for higher limits.
- **Exa**: anonymous casual use; `EXA_API_KEY` is optional.
- **Ollama**: requires `OLLAMA_API_KEY` or the existing `ollama-cloud` auth entry.
- **Custom MCP**: select a server/tool configured in `mcp.json`.

Credentials are never written to `config.json`. They remain in environment variables or the existing provider auth store.

## Skills

Skills are extra instruction packs the model can load on demand. Put a `SKILL.md` in a skill directory (YAML frontmatter with `name` and `description`, then markdown). User-level:

```
$XDG_CONFIG_HOME/atom/skills/*/SKILL.md   # default ~/.config/atom/skills
~/.agents/skills/*/SKILL.md
~/.cursor/skills/*/SKILL.md
```

Project-level (walk from the session cwd up to home; closest name wins): `.atom/skills`, `.cursor/skills`, `.agents/skills`.

## MCP

MCP servers are configured in JSON (`mcpServers` map), Cursor-compatible. Later files override the same server name. Disabled servers and SSE transports are skipped.

```
$XDG_CONFIG_HOME/atom/mcp.json
<dir>/.atom/mcp.json
<dir>/.cursor/mcp.json
```

Stdio servers use `command` + `args` (optional `env`). HTTP servers use `url` (optional `headers`). Tools show up as `mcp_<server>_<tool>`.

## Build

Atom is distributed as one `atom` executable. The same executable runs the TUI
normally and re-invokes itself with `--serve` for the detached background
session server.

### Development

```sh
# Build the debug binary
cargo build --bin atom

# Or use the Makefile to build and symlink into ~/.local/bin
make install-dev
```

Make sure `~/.local/bin` is on your `PATH`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### Release install

```sh
make install              # copies release build to ~/.local/bin
sudo make install PREFIX=/opt/homebrew   # Apple Silicon Homebrew prefix
sudo make install PREFIX=/usr/local      # Intel Mac / traditional prefix
```

### GitHub releases

Tag, build, and attach the binary in one command (requires `gh` authed):

```sh
make release VERSION=v0.1.0
# or: ./scripts/release.sh v0.1.0
```

This runs `cargo build --release --bin atom`, packages the binary as
`atom-<version>-<os>-<arch>.tar.gz`, pushes the tag, and attaches the
archive to the GitHub release. Commit your work first.

Users install with:

```sh
curl -fsSL https://raw.githubusercontent.com/andrewmillercode/atom/main/install.sh | bash
```

The installer detects the platform, grabs the latest release, installs to
`~/.local/bin`, adds it to your PATH, and checks the runtime deps (`rg`,
`uv`). Pin a version or dir with `ATOM_VERSION=v0.1.0` /
`ATOM_INSTALL_DIR=/opt/bin`. Manual alternative:

```sh
curl -fL https://github.com/andrewmillercode/atom/releases/download/v0.1.0/atom-v0.1.0-Darwin-arm64.tar.gz | tar -xz
sudo install atom /usr/local/bin
```

or build from source: clone the repo and `cargo install --path crates/atom`.

