//! Tool definitions, ported from main.go builtinToolDefinitions plus the
//! def builders living in search.go / skills.go / mcp.go / vector_search.go
//! / dispatch.go. Parameter schemas are Go's raw JSON strings verbatim.
//!
//! Descriptions are grug-style: short sentences, no filler, same rules.
//! Serialized defs are charged to the model context every call, so tokens
//! spent here buy rules, not prose. Budget is checked by length: serialized
//! bytes / 4 (same heuristic as atom-core context_breakdown) should stay
//! near 2.5k tokens for the builtin list.

use atom_core::types::ToolDef;

fn def(name: &str, description: &str, parameters: &str) -> ToolDef {
    let params: serde_json::Value = serde_json::from_str(parameters).expect("curated schema");
    ToolDef::new(name, description, params)
}

/// builtinToolDefinitions is the built-in OpenAI tool list, including skill.
pub fn builtin_tool_definitions() -> Vec<ToolDef> {
    vec![
        def(
            "web_search",
            "Search web for current info. Returns titles, URLs, snippets. Use when user asks about current events or info not in training data. Keep query concise, then answer from results.",
            r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query"}},"required":["query"]}"#,
        ),
        def(
            "read_file",
            "Read file, returns window of lines (offset default 0, limit default 1000). After this, edit_file and write_file may be used on path. If file changes on disk later, those tools return short diff — do not re-read whole file. Images (png, jpg, gif, webp, bmp) return as image for vision models.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path, absolute or relative"},"offset":{"type":"integer","description":"0-based line offset, default 0"},"limit":{"type":"integer","description":"Max lines, default 1000"}},"required":["path"]}"#,
        ),
        def(
            "write_file",
            "Create or overwrite file with content. Existing file must be observed with read_file first. Write re-checked automatically; if file changed since last read, write skipped, short diff returned. New file needs no prior read. Do not write files with bash.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path to create or overwrite"},"content":{"type":"string","description":"Full content to write"}},"required":["path","content"]}"#,
        ),
        def(
            "edit_file",
            "Edit file by replacing unique exact text. Call read_file on path once first (window enough). Re-checked automatically; if file changed since last read, edit skipped, short diff returned. After success, do not read_file again unless next edit fails.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path to edit"},"old_text":{"type":"string","description":"Exact text to find in file"},"new_text":{"type":"string","description":"Replacement text"}},"required":["path","old_text","new_text"]}"#,
        ),
        vector_search_def(),
        grep_def(),
        glob_def(),
        visualize_def(),
        def(
            "bash",
            "Run commands needing shell: tests, builds, git, package managers. Last resort — only when no other builtin tool can do the job. Never use bash to search files or web: use glob and grep instead; file reads and changes belong in read_file, write_file, edit_file.",
            r#"{"type":"object","properties":{"command":{"type":"string","description":"Command to run from session workspace"}},"required":["command"]}"#,
        ),
        dispatch_def(),
        skill_def(),
    ]
}

pub fn vector_search_def() -> ToolDef {
    def(
        "vector_search",
        "Search local dir or git URL for code by meaning or identifier. Returns snippets with file paths and line ranges. Prefer over grep to learn how code works. Not web search. Index built on first run, cached, auto-invalidated on file change. Workflow: 1. vector_search first. 2. Open returned file at given line — no re-search or grep for same content. 3. grep only for every literal or regex occurrence (e.g. all callers of renamed fn). 4. glob for files by name or pattern. 5. No bash (find, fd, ls, grep, rg) for file search.",
        r#"{"type":"object","properties":{"query":{"type":"string","description":"Natural-language or identifier query"},"path":{"type":"string","description":"Local dir or https git URL, default cwd"},"content":{"type":"string","enum":["code","docs","config","all"],"description":"What to search, default code"},"top_k":{"type":"integer","description":"Max snippets"},"max_snippet_lines":{"type":"integer","description":"Show only first N lines of snippet, 0 = path and line range only"}},"required":["query"]}"#,
    )
}

pub fn grep_def() -> ToolDef {
    def(
        "grep",
        "Fast regex text search: path, line number, matched text. Pattern regex by default; regex=false for literal. Use for identifiers, error strings, config keys, structural patterns instead of bash grep or rg. Searches session workspace by default, honors .gitignore.",
        r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Regex to find; regex=false for literal substring, default regex mode"},"path":{"type":"string","description":"Optional file or dir, relative to workspace or absolute; omit = workspace"},"glob":{"type":"string","description":"File filter like *.rs or **/*.test.ts"},"regex":{"type":"boolean","description":"false = literal substring, default true"},"case_insensitive":{"type":"boolean","description":"Force case-insensitive, else smart-case"},"head_limit":{"type":"integer","description":"Max matches, default 100"}},"required":["pattern"]}"#,
    )
}

pub fn glob_def() -> ToolDef {
    def(
        "glob",
        "Fast file discovery by pattern, instead of bash find, fd, ls. Searches session workspace recursively, honors .gitignore.",
        r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Glob like **/*.rs, src/**/*.md, Cargo.toml"},"path":{"type":"string","description":"Optional dir, relative to workspace or absolute; omit = workspace"},"head_limit":{"type":"integer","description":"Max paths, default 200"}},"required":["pattern"]}"#,
    )
}

pub fn visualize_def() -> ToolDef {
    def(
        "visualize",
        r#"Render Mermaid diagram inline in atom TUI, high-density image at full block width; click opens pan/zoom browser viewer. Use for concept thinking, architecture, data flow, call graphs, sequence, state machines, ER. Prefer over ASCII-art. Not for math unless user asks. House style: muted colors, color on borders only — classDef fill:none plus colored stroke, never bright fills (dark background); compact landscape layout, flowchart LR, short labels, side-by-side subgraphs. Source is raw Mermaid, no fences, complete diagram in one call, short title (names browser tab and artifact file). Render failure names offending line — fix and retry, no giving up. Inline image needs kitty graphics terminal; else open named HTML file in browser.

Few-shot, most render failures are unquoted labels. BAD (colon breaks parser, bright style fill off-style, TD wastes space):
A[Layer 1: static rules]
GOOD:
flowchart LR
  A[Tool call] --> B{"Layer 1<br/>static rules"}
  B -->|allow| C["run confined<br/>Seatbelt on"]
  classDef plain fill:none,stroke:#8a919c
  class A,B,C plain
Rules: quote labels with punctuation, <br/> for line breaks, word-only labels may stay unquoted, one classDef per color role with fill:none + muted stroke, LR, short labels."#,
        r#"{"type":"object","properties":{"code":{"type":"string","description":"Mermaid source, e.g. 'flowchart LR\\n  A[Start] --> B[Done]'"},"title":{"type":"string","description":"Short title, names browser tab and artifact file"}},"required":["code"]}"#,
    )
}

pub fn dispatch_def() -> ToolDef {
    def(
        "dispatch",
        "Manage subagents through one bulk interface. action=models: discover exact providers and model IDs. action=spawn: one subagent per tasks string (max 100, min 1); tasks accepts a single string or a list of strings (a string is treated as a one-item list), all share provider/model/thinking; result has session id; send new prompt to continue, cancel to stop. spawn requires at least one task. action=inspect: status snapshot and optional results for ids, batch_id, or all owned subagents. action=send: continue selected subagents with prompt, or distinct messages per subagent. action=cancel: stop selected. Every operation bulk-capable; prefer batch_id over many IDs. User can open subagent by clicking tool block or shift+down. No nested dispatch, one level only.",
        r#"{"type":"object","properties":{"action":{"type":"string","enum":["models","spawn","inspect","send","cancel"],"description":"Operation."},"tasks":{"anyOf":[{"type":"string"},{"type":"array","minItems":1,"maxItems":100,"items":{"type":"string"}}],"description":"Spawn only: one prompt per new subagent; a single string is treated as a one-item list."},"provider":{"type":"string","description":"Spawn only: exact provider from action=models. Omit to inherit."},"model":{"type":"string","description":"Spawn only: exact model ID from action=models. Omit to inherit."},"thinking":{"type":"string","description":"Spawn/send reasoning_effort: none, low, high, or max."},"batch_id":{"type":"string","description":"Target every subagent from one spawn."},"ids":{"type":"array","items":{"type":"string"},"description":"Selected subagent IDs. Omit ids and batch_id to target all owned."},"prompt":{"type":"string","description":"Send only: follow-up prompt shared by all targets."},"messages":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"prompt":{"type":"string"}},"required":["id","prompt"]},"description":"Send only: distinct follow-up prompts by subagent ID."},"wait":{"type":"string","enum":["none","any","all"],"description":"Spawn/inspect: when to return, default none."},"results":{"type":"boolean","description":"Inspect: include available results, default true."},"statuses":{"type":"array","items":{"type":"string","enum":["queued","working","sandbox","error","done","cancelled"]},"description":"Cancel only: restrict cancellation to these statuses."},"query":{"type":"string","description":"Models only: optional model ID filter."}},"required":["action"]}"#,
    )
}

pub fn skill_def() -> ToolDef {
    def(
        "skill",
        "Load skill instructions by exact name from skills catalog (system prompt lists name + description only). Call when request matches listed skill, then follow instructions. Never load irrelevant skill.",
        r#"{"type":"object","properties":{"name":{"type":"string","description":"Exact skill name from catalog"}},"required":["name"]}"#,
    )
}

/// withoutTool returns a copy of tools with the named function removed.
pub fn without_tool(tools: &[ToolDef], name: &str) -> Vec<ToolDef> {
    tools
        .iter()
        .filter(|t| t.function.name != name)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_put_search_before_shell() {
        let names: Vec<String> = builtin_tool_definitions()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                "web_search",
                "read_file",
                "write_file",
                "edit_file",
                "vector_search",
                "grep",
                "glob",
                "visualize",
                "bash",
                "dispatch",
                "skill",
            ]
        );
        assert_eq!(crate::tool_definitions().len(), 11);
    }

    #[test]
    fn search_tools_precede_and_specialize_bash() {
        for t in crate::tool_definitions() {
            if t.function.name == "bash" {
                assert!(t.function.description.contains("glob and grep"));
            }
            if t.function.name == "grep" {
                assert_eq!(t.kind, "function");
                assert_eq!(
                    t.function.parameters["required"][0],
                    serde_json::json!("pattern")
                );
            }
        }
    }

    #[test]
    fn without_tool_strips_and_is_idempotent() {
        let tools = without_tool(&crate::tool_definitions(), "dispatch");
        assert!(tools.iter().all(|t| t.function.name != "dispatch"));
        assert_eq!(without_tool(&tools, "dispatch").len(), tools.len());
        // Stripping everything but keeping order otherwise.
        let no_bash = without_tool(&crate::tool_definitions(), "bash");
        assert_eq!(no_bash.len(), 10);
        assert_eq!(no_bash[4].function.name, "vector_search");
    }
}
