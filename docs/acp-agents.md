# ACP agents — run external agents inside atom

atom speaks the [Agent Client Protocol](https://agentclientprotocol.com) (ACP,
JSON-RPC 2.0 over stdio), the same protocol Zed and JetBrains use. An ACP agent
owns its own model loop and tools; atom renders its output, surfaces its
permission prompts in the existing approval flow, and persists the transcript
like any other session.

## Setup

The bundled agents (Claude Code, Codex, Gemini CLI, Devin) can also be added without
editing any file: open `/providers`, pick the agent row (ACP badge), press
Enter — the launch command is written to `acp.json` for you. `d` removes it.

Declare agents in `~/.config/atom/acp.json` (user level) or `.atom/acp.json`
(project level, overrides user):

```json
{
  "acpAgents": {
    "claude-code": {
      "command": "npx",
      "args": ["-y", "@agentclientprotocol/claude-agent-acp"]
    },
    "codex": {
      "command": "npx",
      "args": ["-y", "@agentclientprotocol/codex-acp"]
    },
    "gemini": {
      "command": "gemini",
      "args": ["--experimental-acp"]
    },
    "devin": {
      "command": "devin",
      "args": ["acp"]
    }
  }
}
```

Same shape as `mcp.json`: `command`, `args`, `env` (with `{env:NAME}` refs),
`disabled` to remove an inherited entry.

## Use

Agents appear in the model picker (`provider` column `acp-agent`). Selecting
one creates the session with provider `acp-agent` and model = agent name; every
prompt is sent to the agent over ACP instead of a model API. Later prompts on
the same session resume the agent-side session (`session/load` when the agent
supports it, otherwise context restarts with a fresh agent session).

Model rows show the agent's own display names (Sonnet 5.5, Opus 5.5, …), and
the agent's `thought_level` option (Claude `effort`, Codex `reasoning_effort`)
drives the thinking ladder. The old `@zed-industries/claude-code-acp` package
is deprecated and only advertises a `default` model; existing configs naming
it are rewritten to the new package at load time.

## What atom serves for the agent

- `session/request_permission` — routed through atom's approval prompts
  (allow once / allow for session / deny), so external agents honor the same
  approval UX as atom's own bash tool. Without a live turn, permission
  requests are refused.
- `fs/read_text_file` / `fs/write_text_file` — served directly; paths must be
  absolute per the spec.
- `terminal` and elicitation are advertised unsupported; agents that rely on
  them degrade to their own fallbacks.

## Event mapping

Agent updates stream through the standard NDJSON event vocabulary, so the TUI
needs nothing new: `agent_message_chunk` → `content`, `agent_thought_chunk` →
`reasoning`, `tool_call`/`tool_call_update` → `tool` + `tool_result` +
`tool_diff` blocks, `plan` → a live "Plan" block. The turn ends on the
prompt's `stopReason`; `Esc` sends `session/cancel`.

Tool calls are renamed onto atom's own tools (`bash`, `read_file`,
`edit_file`, `grep`, …) so highlighting and diffs render as in normal mode.
`usage_update` (context used/size) and the prompt's `usage` block drive the
footer context meter and token totals.

## Subagents

Top-level ACP sessions are started with an `atom` MCP server
(`atom -mcp-bridge`, stdio) exposing atom's `subagent` tool; the agent sees it
as `mcp__atom__subagent` and a hint steers it away from its built-in Task
tool. Subagents are ordinary atom sessions on the model from
`/settings → Subagent model` (ACP sessions have no atom model to inherit),
show in the children panel, forward approvals to the parent, stop on `Esc`,
and wake the agent with a follow-up turn when they finish.

Not wired yet: `session/set_mode` surface in the UI and agent
`available_commands_update` slash commands.