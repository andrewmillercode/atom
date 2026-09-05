//! Customization reference: a single-shot knowledge injection that the
//! `customize` tool returns to the model. The body is a plain markdown
//! reference of every way to extend atom (skills, MCP, AGENTS.md,
//! config, bundled prompt). Mirrors skills.rs in shape — static body,
//! parameterless execute function — but is not itself a SKILL.md.
//!
//! Keep the body factual and terse: paths, schemas, reload semantics.
//! The model drives edits with write_file/edit_file/bash; the tool
//! only injects knowledge.

/// CUSTOMIZE_BODY is the markdown returned by the `customize` tool.
/// Loaded once per call; not stored in the system prompt. Kept terse
/// on purpose — paths, schemas, reload semantics, and the
/// can't-customize footer. Internal build details (release/dev split,
/// compile-time embedding, source-file references) are deliberately
/// omitted.
pub const CUSTOMIZE_BODY: &str = r#"# Customizing atom

Every customization surface is a plain file in a well-known location.
Use the bundled tools (`write_file`, `edit_file`, `bash`) to edit
them. Never paste credentials into chat or commits. Most changes
need an atom restart to take effect.

## Paths

The "atom config dir" is `$XDG_CONFIG_HOME/atom`, defaulting to
`~/.config/atom`. Several surfaces walk **from cwd up to home (or
just cwd if cwd is outside home)**, closest-to-cwd wins, atom
config dir is the fallback.

## Skills — task-specific instruction packs

Loaded on demand via the `skill` tool. Catalog appears in the
system prompt; body loads only when invoked.

- File: `<dir>/skills/<name>/SKILL.md`
- Discovery: `<atom config>/skills/`, `~/.agents/skills/`,
  `~/.cursor/skills/`, then `.atom/skills/`, `.cursor/skills/`,
  `.agents/skills/` walking up from cwd.
- Format: YAML frontmatter (`name`, `description`) + markdown body.
  The description is what the model sees in the catalog — keep it
  terse and trigger-phrase friendly.

```markdown
---
name: <kebab-case>
description: One or two sentences on when to invoke. Mention trigger phrases.
---

# body in markdown
```

Restart atom to apply.

## MCP servers — external tool integrations

Each entry becomes model tools prefixed `mcp_<server>_<tool>`.

- File: `<dir>/mcp.json`
- Discovery: `<atom config>/mcp.json`, then `.atom/mcp.json` and
  `.cursor/mcp.json` walking up from cwd.
- Schema:

```json
{
  "mcpServers": {
    "<name>": {
      "command": "<executable>",
      "args": ["..."],
      "env": { "KEY": "value" },
      "url": "https://...   (for HTTP/streamable)",
      "headers": { "Authorization": "Bearer ..." },
      "disabled": false,
      "type": "stdio | http",
      "client_id": "...",
      "client_secret": "...",
      "auth": "oauth",
      "defer": false
    }
  }
}
```

- `"disabled": true` skips the server without deleting it.
- Env values support `{env:NAME}` token expansion.
- Servers exposing >20 tools auto-defer to `find_tool`; set
  `"defer": true` to force, `false` to override.
- `"auth": "oauth"` opts into interactive browser sign-in on 401.
  Static `client_id`/`client_secret` skip dynamic client
  registration.

Restart atom to apply.

## AGENTS.md — project + global rules

Project-local rules merged into every session's system prompt.

- File: `<dir>/AGENTS.md` (plain markdown).
- Discovery: `<atom config>/AGENTS.md` first, then walks from cwd
  up to home, closest first. Outside home, only cwd is checked.
- Use for repo-specific instructions: build commands, test patterns,
  code conventions. Keep it short — it ships every turn.

Restart atom to apply.

## Config (`config.json`) — session-wide settings

- File: `<atom config>/config.json`
- Schema (every field optional; unknown fields ignored):

```json
{
  "version": 1,
  "compaction": { "provider": "...", "model": "..." },
  "web_search": { "server": "...", "tool": "..." },
  "auto_update": true,
  "theme": "<theme id>"
}
```

Restart atom to apply.

## Themes — UI color overrides

- Built-in: ships with atom; no install step.
- User: `<atom config>/themes/<id>.json`.
- Format: flat key/value object with color slots —
  `background`, `foreground`, `primary`, `secondary`, `border`,
  `card_dark`, `card_light`, `select`, `muted`, `syntax_*`, `diff_*`.
- Activate by setting `"theme": "<id>"` in `config.json` to the
  file stem (no `.json`).

Restart atom to apply.

## Bundled system prompt

Lives at `instructions/system-prompt.md` in the atom repository.
Edit the file and rebuild atom for changes to take effect — a
plain restart is not enough.

## What you cannot change without source edits

- The built-in tool set.
- Built-in themes.
- The bundled system prompt itself.
- Discovery orders for skills, MCP, and AGENTS.md.

For everything else: drop the right file at the right path and
restart.
"#;

/// execute_customize returns the customization reference. Takes no
/// arguments. Diagnostic messages mirror skills::execute_skill for
/// consistency (so callers that grep on `error parsing arguments`
/// keep working).
pub fn execute_customize(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        // Empty args is the normal call shape for this tool. Don't
        // return the "model emitted no arguments" diagnostic that
        // empty_arguments_msg would produce for tools that actually
        // need params.
        return CUSTOMIZE_BODY.to_string();
    }
    // We accept anything that parses as a JSON object (even `{}`).
    // Anything else is a real error.
    let v: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    if !v.is_object() {
        return format!("error parsing arguments: customize takes no arguments, got {v}");
    }
    CUSTOMIZE_BODY.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_args_returns_body() {
        let s = execute_customize("");
        assert!(s.contains("# Customizing atom"));
        assert!(s.contains("Skills"));
        assert!(s.contains("MCP servers"));
        assert!(s.contains("AGENTS.md"));
        assert!(s.contains("Themes"));
        assert!(s.contains("Bundled system prompt"));
    }

    #[test]
    fn whitespace_args_returns_body() {
        let s = execute_customize("   \n  ");
        assert!(s.contains("# Customizing atom"));
    }

    #[test]
    fn empty_object_returns_body() {
        let s = execute_customize("{}");
        assert!(s.contains("# Customizing atom"));
    }

    #[test]
    fn unknown_field_returns_body() {
        // Future-proofing: if we ever accept params, ignore extra ones.
        let s = execute_customize(r#"{"section":"skills"}"#);
        assert!(s.contains("# Customizing atom"));
    }

    #[test]
    fn non_object_args_error() {
        let s = execute_customize("[]");
        assert!(s.starts_with("error parsing arguments"));
        let s2 = execute_customize("42");
        assert!(s2.starts_with("error parsing arguments"));
    }

    #[test]
    fn invalid_json_error() {
        let s = execute_customize("not json");
        assert!(s.starts_with("error parsing arguments"));
    }

    #[test]
    fn body_mentions_all_paths() {
        let body = CUSTOMIZE_BODY;
        // User-visible paths the model needs to write to / read from.
        assert!(body.contains("~/.config/atom"), "config dir path");
        assert!(body.contains("~/.agents/skills"), "agents skills dir");
        assert!(body.contains("~/.cursor/skills"), "cursor skills dir");
        assert!(body.contains(".atom/mcp.json"), "project mcp config");
        assert!(body.contains("config.json"), "config file");
        assert!(
            body.contains("instructions/system-prompt.md"),
            "bundled prompt path"
        );

        // Internal build details must not leak into the body.
        assert!(
            !body.contains("atom-dev"),
            "release/dev split is internal — drop it"
        );
        assert!(
            !body.contains("ui/themes/"),
            "built-in themes location is internal — drop it"
        );
        assert!(
            !body.contains("include_str!"),
            "compile-time embedding is internal — drop it"
        );
        assert!(
            !body.contains("crates/atom-tools/"),
            "source-file references are internal — drop them"
        );
    }

    #[test]
    fn body_keeps_critical_reload_semantics() {
        // The reload requirement is the most common user failure mode;
        // every restart-required section must call it out.
        let body = CUSTOMIZE_BODY;
        let restart_lines = body.matches("Restart atom to apply.").count();
        assert!(
            restart_lines >= 5,
            "expected at least 5 'Restart atom to apply.' lines, got {restart_lines}"
        );
        // The bundled prompt needs a rebuild, not a restart — that
        // distinction is critical and must remain in the body.
        assert!(
            body.contains("rebuild atom"),
            "bundled prompt section must mention rebuild"
        );
        assert!(
            body.contains("plain restart is not enough"),
            "bundled prompt must distinguish rebuild from restart"
        );
    }
}
