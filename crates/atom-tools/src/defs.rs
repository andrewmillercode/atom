//! Tool definitions, ported from main.go builtinToolDefinitions plus the
//! def builders living in search.go / skills.go / mcp.go / vector_search.go
//! / dispatch.go. Parameter schemas are Go's raw JSON strings verbatim.

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
            "Search the web for current information. Returns search result titles, URLs, and snippets.",
            r#"{"type":"object","properties":{"query":{"type":"string","description":"The search query"}},"required":["query"]}"#,
        ),
        def(
            "read_file",
            "Read a file. Returns a window of lines (offset defaults to 0, limit defaults to 1000). After this, edit_file and write_file may be used on the path. If the file later changes on disk, those tools return a short diff of the change — do not re-read the whole file. Image files (png, jpg, gif, webp, bmp) are returned as images so vision-capable models can see them.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to read"},"offset":{"type":"integer","description":"0-based line offset to start reading from. Defaults to 0."},"limit":{"type":"integer","description":"Maximum number of lines to return. Defaults to 1000."}},"required":["path"]}"#,
        ),
        def(
            "write_file",
            "Create or overwrite a file with the given content. Existing files must be observed with read_file first. The file is re-checked automatically; if it changed since the last observation, the write is skipped and a short diff is returned. New files do not need a prior read. Do not use bash to write files.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to create or overwrite"},"content":{"type":"string","description":"The full content to write to the file"}},"required":["path","content"]}"#,
        ),
        def(
            "edit_file",
            "Edit a file by replacing unique exact text. Call read_file on the path once first (a window is enough). The file is re-checked automatically. If it changed since the last observation, the edit is skipped and a short diff is returned. After a successful edit, do not read_file again unless the next edit fails.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to edit"},"old_text":{"type":"string","description":"The exact text to find in the file"},"new_text":{"type":"string","description":"The replacement text"}},"required":["path","old_text","new_text"]}"#,
        ),
        vector_search_def(),
        grep_def(),
        glob_def(),
        visualize_def(),
        def(
            "bash",
            "Run commands that require a shell, such as tests, builds, git, and package managers. File discovery and text search are faster and safer with glob and grep; file reads and changes belong in read_file, write_file, and edit_file.",
            r#"{"type":"object","properties":{"command":{"type":"string","description":"Command to execute from the session workspace"}},"required":["command"]}"#,
        ),
        dispatch_def(),
        skill_def(),
    ]
}

pub fn vector_search_def() -> ToolDef {
    def(
        "vector_search",
        "Search a local directory or git URL for relevant code by meaning or identifier. Returns matching snippets with file paths and line ranges. Prefer this over grep when looking for how something works. Does not search the web.",
        r#"{"type":"object","properties":{"query":{"type":"string","description":"Natural-language or identifier query"},"path":{"type":"string","description":"Local directory or https git URL. Defaults to the current directory."},"content":{"type":"string","enum":["code","docs","config","all"],"description":"What to search. Defaults to code."},"top_k":{"type":"integer","description":"Maximum number of snippets to return"},"max_snippet_lines":{"type":"integer","description":"Show only the first N lines of each snippet. 0 returns path and line range only."}},"required":["query"]}"#,
    )
}

pub fn grep_def() -> ToolDef {
    def(
        "grep",
        "Fast exact text search with path, line number, and matching text. Use for identifiers, error strings, config keys, and regexes instead of running grep or rg in bash. The session workspace is searched by default and .gitignore is honored.",
        r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Text to find. Literal by default, so common code symbols need no escaping."},"path":{"type":"string","description":"Optional file or directory, relative to the session workspace or an absolute path. Omit to search the workspace."},"glob":{"type":"string","description":"Optional file filter such as *.rs or **/*.test.ts"},"regex":{"type":"boolean","description":"Enable regular-expression matching. Defaults to false for fast literal search."},"case_insensitive":{"type":"boolean","description":"Force case-insensitive matching. Otherwise smart-case is used."},"head_limit":{"type":"integer","description":"Maximum matches returned. Defaults to 100."}},"required":["pattern"]}"#,
    )
}

pub fn glob_def() -> ToolDef {
    def(
        "glob",
        "Fast file discovery by pattern. Use instead of find, fd, or ls in bash. Searches the session workspace recursively by default and honors .gitignore.",
        r#"{"type":"object","properties":{"pattern":{"type":"string","description":"File glob such as **/*.rs, src/**/*.md, or Cargo.toml"},"path":{"type":"string","description":"Optional directory relative to the session workspace or an absolute path. Omit to search the workspace."},"head_limit":{"type":"integer","description":"Maximum paths returned. Defaults to 200."}},"required":["pattern"]}"#,
    )
}

pub fn visualize_def() -> ToolDef {
    def(
        "visualize",
        "Render a Mermaid diagram inline in the atom TUI as a high-density image, with an expand button (top-right of the rendered diagram) that opens a pan/zoom viewer in the browser. Use for conceptual thinking, architecture, data flow, function call graphs, sequence diagrams, state machines, ER models, etc. Prefer this over ASCII-art diagrams in replies. Provide standard Mermaid source (flowchart, sequenceDiagram, classDiagram, stateDiagram-v2, erDiagram, mindmap, gantt, pie, journey, gitGraph, and more). Include a short title — it names the browser artifact. If a render fails, the error names the offending line; fix the source and retry rather than giving up.",
        r#"{"type":"object","properties":{"code":{"type":"string","description":"The Mermaid diagram source, e.g. 'flowchart TD\\n  A[Start] --> B[Done]'"},"title":{"type":"string","description":"Short title for the diagram (used for the browser tab and artifact filename)"}},"required":["code"]}"#,
    )
}

pub fn dispatch_def() -> ToolDef {
    def(
        "dispatch",
        "Manage subagents through one bulk interface. Use action=models to discover exact providers and model IDs. action=spawn creates one subagent per string in tasks (maximum 100), all sharing provider/model/thinking; wait may be none, any, or all. action=inspect returns a status snapshot and optional results for ids, a batch_id, or all owned subagents. action=send continues selected subagents with prompt, or accepts messages for different prompts. action=cancel stops selected subagents. Every operation is bulk-capable; prefer batch_id over carrying many IDs. Nested dispatch is not allowed.",
        r#"{"type":"object","properties":{"action":{"type":"string","enum":["models","spawn","inspect","send","cancel"],"description":"Operation to perform."},"tasks":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"string"},"description":"Spawn only: one prompt string per new subagent."},"provider":{"type":"string","description":"Spawn only: exact provider from action=models. Omit to inherit."},"model":{"type":"string","description":"Spawn only: exact model ID from action=models. Omit to inherit."},"thinking":{"type":"string","description":"Spawn/send reasoning_effort, such as none, low, high, or max."},"batch_id":{"type":"string","description":"Target every subagent created by one spawn operation."},"ids":{"type":"array","items":{"type":"string"},"description":"Target selected subagent IDs. Omit ids and batch_id to target all owned subagents."},"prompt":{"type":"string","description":"Send only: follow-up prompt shared by all targets."},"messages":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"prompt":{"type":"string"}},"required":["id","prompt"]},"description":"Send only: distinct follow-up prompts by subagent ID."},"wait":{"type":"string","enum":["none","any","all"],"description":"Spawn/inspect: when to return. Defaults to none."},"results":{"type":"boolean","description":"Inspect: include each available result. Defaults to true."},"statuses":{"type":"array","items":{"type":"string","enum":["queued","working","sandbox","error","done","cancelled"]},"description":"Cancel only: restrict cancellation to these statuses."},"query":{"type":"string","description":"Models only: optional model ID filter."}},"required":["action"]}"#,
    )
}

pub fn skill_def() -> ToolDef {
    def(
        "skill",
        "Load a skill's full instructions by exact name from the skills catalog. Call this when the user's request matches a listed skill. Then follow those instructions.",
        r#"{"type":"object","properties":{"name":{"type":"string","description":"Exact skill name from the catalog"}},"required":["name"]}"#,
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
