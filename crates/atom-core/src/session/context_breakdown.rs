//! Live model-context occupancy estimate by category, ported from
//! context.go. Pure functions only.

use crate::session::compaction::llm_messages;
use crate::session::store::Session;
use crate::types::{Message, ToolDef};

/// contextRow is one category in the /context footer menu.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextRow {
    pub name: String,
    pub tokens: i64,
    pub pct: i64,
}

pub const CONTEXT_CAT_ATOM: &str = "atom";
pub const CONTEXT_CAT_REPO: &str = "repo";
pub const CONTEXT_CAT_TOOLS: &str = "tools";
pub const CONTEXT_CAT_USER: &str = "user";
pub const CONTEXT_CAT_AGENT: &str = "agent";

pub const CONTEXT_CATEGORY_ORDER: [&str; 5] = [
    CONTEXT_CAT_ATOM,
    CONTEXT_CAT_REPO,
    CONTEXT_CAT_TOOLS,
    CONTEXT_CAT_USER,
    CONTEXT_CAT_AGENT,
];

/// estimateTokens is the char/4 heuristic used for session usage.
pub fn estimate_tokens(n: usize) -> i64 {
    ((n + 3) / 4) as i64
}

pub fn estimate_message_chars(m: &Message) -> i64 {
    let mut n = m.content.len() + m.reasoning.len();
    for tc in &m.tool_calls {
        n += tc.function.name.len() + tc.function.arguments.len();
    }
    n as i64
}

fn first_line(s: &str) -> &str {
    s.split('\n').next().unwrap_or(s)
}

pub fn instruction_source(content: &str) -> String {
    let line = first_line(content);
    const PREFIX: &str = "Instructions from: ";
    match line.strip_prefix(PREFIX) {
        Some(rest) => rest.trim().to_string(),
        None => String::new(),
    }
}

/// classifyInstruction maps a system instruction to atom or repo.
/// Unknown prefixes count as atom.
pub fn classify_instruction(content: &str) -> &'static str {
    // filepath.ToSlash is a no-op on unix-style paths.
    let src = instruction_source(content).replace('\\', "/");
    if src == "skills" {
        return CONTEXT_CAT_REPO;
    }
    if src == "TOOLS.md" {
        return CONTEXT_CAT_ATOM;
    }
    if src.contains("/atom/AGENTS.md") || src.ends_with("atom/AGENTS.md") {
        return CONTEXT_CAT_ATOM;
    }
    if src.ends_with("AGENTS.md") {
        return CONTEXT_CAT_REPO;
    }
    CONTEXT_CAT_ATOM
}

/// allocatePercents splits 100 across parts with largest-remainder
/// rounding; ties break toward the earlier index. All-zero parts stay
/// zero (no 100% row for an empty context).
pub fn allocate_percents(parts: &[i64]) -> Vec<i64> {
    let mut out = vec![0i64; parts.len()];
    let total: i64 = parts.iter().sum();
    if total == 0 {
        return out;
    }
    #[derive(Clone, Copy)]
    struct Rem {
        i: usize,
        frac: i64,
    }
    let mut rems = Vec::with_capacity(parts.len());
    let mut used = 0i64;
    for (i, p) in parts.iter().enumerate() {
        let q = p * 100 / total;
        out[i] = q;
        used += q;
        rems.push(Rem {
            i,
            frac: p * 100 % total,
        });
    }
    let leftover = 100 - used;
    rems.sort_by(|a, b| b.frac.cmp(&a.frac).then(a.i.cmp(&b.i)));
    for k in 0..leftover.min(rems.len() as i64) as usize {
        out[rems[k].i] += 1;
    }
    out
}

/// contextBreakdown estimates live model-context occupancy by category.
/// Tool definitions are the builtins; pass MCP tool defs via
/// [`context_breakdown_with_tools`] once the server discovers them
/// (Go's toolDefinitionsFor(cwd) = builtins + MCP tools for cwd).
pub fn context_breakdown(sess: &Session) -> Vec<ContextRow> {
    context_breakdown_with_tools(sess, &[])
}

pub fn context_breakdown_with_tools(sess: &Session, extra_tools: &[ToolDef]) -> Vec<ContextRow> {
    let mut chars: std::collections::HashMap<&'static str, usize> =
        CONTEXT_CATEGORY_ORDER.iter().map(|&c| (c, 0)).collect();

    for m in &sess.instructions {
        *chars.entry(classify_instruction(&m.content)).or_insert(0) += m.content.len();
    }

    let mut tools = builtin_tool_definitions();
    tools.extend_from_slice(extra_tools);
    if let Ok(raw) = serde_json::to_vec(&tools) {
        *chars.get_mut(CONTEXT_CAT_TOOLS).unwrap() += raw.len();
    }

    for m in llm_messages(sess) {
        if m.role == "system" {
            continue;
        }
        let n = estimate_message_chars(&m) as usize;
        if m.role == "user" {
            *chars.get_mut(CONTEXT_CAT_USER).unwrap() += n;
        } else {
            *chars.get_mut(CONTEXT_CAT_AGENT).unwrap() += n;
        }
    }

    let toks: Vec<i64> = CONTEXT_CATEGORY_ORDER
        .iter()
        .map(|name| estimate_tokens(chars[name]))
        .collect();
    let pcts = allocate_percents(&toks);
    let mut rows = Vec::with_capacity(CONTEXT_CATEGORY_ORDER.len());
    for (i, name) in CONTEXT_CATEGORY_ORDER.iter().enumerate() {
        rows.push(ContextRow {
            name: (*name).to_string(),
            tokens: toks[i],
            pct: pcts[i],
        });
    }
    rows
}

pub fn context_row_meta(row: &ContextRow) -> String {
    format!(
        "{}  {:>3}%",
        crate::session::stats::format_tokens(row.tokens),
        row.pct
    )
}

/// builtinToolDefinitions is the built-in OpenAI tool list, including
/// skill (main.go and friends). Only its serialized JSON length is used
/// here today, but the definitions are byte-faithful ports so other
/// crates can reuse them.
pub fn builtin_tool_definitions() -> Vec<ToolDef> {
    use serde_json::json;
    macro_rules! def {
        ($name:expr, $desc:expr, $params:expr) => {
            ToolDef::new($name, $desc, $params)
        };
    }
    vec![
        def!(
            "web_search",
            "Search the web for current information. Returns search result titles, URLs, and snippets.",
            json!({"type":"object","properties":{"query":{"type":"string","description":"The search query"}},"required":["query"]})
        ),
        def!(
            "read_file",
            "Read a file. Returns a window of lines (offset defaults to 0, limit defaults to 1000). After this, edit_file and write_file may be used on the path. If the file later changes on disk, those tools return a short diff of the change — do not re-read the whole file. Image files (png, jpg, gif, webp, bmp) are returned as images so vision-capable models can see them.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to read"},"offset":{"type":"integer","description":"0-based line offset to start reading from. Defaults to 0."},"limit":{"type":"integer","description":"Maximum number of lines to return. Defaults to 1000."}},"required":["path"]})
        ),
        def!(
            "write_file",
            "Create or overwrite a file with the given content. Existing files must be observed with read_file first. The file is re-checked automatically; if it changed since the last observation, the write is skipped and a short diff is returned. New files do not need a prior read. Do not use bash to write files.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to create or overwrite"},"content":{"type":"string","description":"The full content to write to the file"}},"required":["path","content"]})
        ),
        def!(
            "edit_file",
            "Edit a file by replacing unique exact text. Call read_file on the path once first (a window is enough). The file is re-checked automatically. If it changed since the last observation, the edit is skipped and a short diff is returned. After a successful edit, do not read_file again unless the next edit fails.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to edit"},"old_text":{"type":"string","description":"The exact text to find in the file"},"new_text":{"type":"string","description":"The replacement text"}},"required":["path","old_text","new_text"]})
        ),
        def!(
            "vector_search",
            "Search a local directory or git URL for relevant code by meaning or identifier. Returns matching snippets with file paths and line ranges. Prefer this over grep when looking for how something works. Does not search the web.",
            json!({"type":"object","properties":{"query":{"type":"string","description":"Natural-language or identifier query"},"path":{"type":"string","description":"Local directory or https git URL. Defaults to the current directory."},"content":{"type":"string","enum":["code","docs","config","all"],"description":"What to search. Defaults to code."},"top_k":{"type":"integer","description":"Maximum number of snippets to return"},"max_snippet_lines":{"type":"integer","description":"Show only the first N lines of each snippet. 0 returns path and line range only."}},"required":["query"]})
        ),
        def!(
            "grep",
            "Fast exact text search with path, line number, and matching text. Use for identifiers, error strings, config keys, and regexes instead of running grep or rg in bash. The session workspace is searched by default and .gitignore is honored.",
            json!({"type":"object","properties":{"pattern":{"type":"string","description":"Text to find. Literal by default, so common code symbols need no escaping."},"path":{"type":"string","description":"Optional file or directory, relative to the session workspace. Omit to search the workspace."},"glob":{"type":"string","description":"Optional file filter such as *.rs or **/*.test.ts"},"regex":{"type":"boolean","description":"Enable regular-expression matching. Defaults to false for fast literal search."},"case_insensitive":{"type":"boolean","description":"Force case-insensitive matching. Otherwise smart-case is used."},"head_limit":{"type":"integer","description":"Maximum matches returned. Defaults to 100."}},"required":["pattern"]})
        ),
        def!(
            "glob",
            "Fast file discovery by pattern. Use instead of find, fd, or ls in bash. Searches the session workspace recursively by default and honors .gitignore.",
            json!({"type":"object","properties":{"pattern":{"type":"string","description":"File glob such as **/*.rs, src/**/*.md, or Cargo.toml"},"path":{"type":"string","description":"Optional directory relative to the session workspace. Omit to search the workspace."},"head_limit":{"type":"integer","description":"Maximum paths returned. Defaults to 200."}},"required":["pattern"]})
        ),
        def!(
            "visualize",
            "Render a Mermaid diagram inline in the atom TUI as a high-density image, with an expand button (top-right of the rendered diagram) that opens a pan/zoom viewer in the browser. Use for conceptual thinking, architecture, data flow, function call graphs, sequence diagrams, state machines, ER models, etc. Prefer this over ASCII-art diagrams in replies. Provide standard Mermaid source (flowchart, sequenceDiagram, classDiagram, stateDiagram-v2, erDiagram, mindmap, gantt, pie, journey, gitGraph, and more). Include a short title — it names the browser artifact. If a render fails, the error names the offending line; fix the source and retry rather than giving up.",
            json!({"type":"object","properties":{"code":{"type":"string","description":"The Mermaid diagram source, e.g. 'flowchart TD\\n  A[Start] --> B[Done]'"},"title":{"type":"string","description":"Short title for the diagram (used for the browser tab and artifact filename)"}},"required":["code"]})
        ),
        def!(
            "bash",
            "Run commands that require a shell, such as tests, builds, git, and package managers. File discovery and text search are faster and safer with glob and grep; file reads and changes belong in read_file, write_file, and edit_file.",
            json!({"type":"object","properties":{"command":{"type":"string","description":"Command to execute from the session workspace"}},"required":["command"]})
        ),
        def!(
            "dispatch",
            "Manage subagents through one bulk interface. Use action=models to discover models; spawn creates one subagent per task string; inspect, send, and cancel target IDs, a batch, or all owned subagents.",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["models","spawn","inspect","send","cancel"]},"tasks":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"string"}},"provider":{"type":"string"},"model":{"type":"string"},"thinking":{"type":"string"},"batch_id":{"type":"string"},"ids":{"type":"array","items":{"type":"string"}},"prompt":{"type":"string"},"messages":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"prompt":{"type":"string"}},"required":["id","prompt"]}},"wait":{"type":"string","enum":["none","any","all"]},"results":{"type":"boolean"},"statuses":{"type":"array","items":{"type":"string"}},"query":{"type":"string"}},"required":["action"]})
        ),
        def!(
            "skill",
            "Load a skill's full instructions by exact name from the skills catalog. Call this when the user's request matches a listed skill. Then follow those instructions.",
            json!({"type":"object","properties":{"name":{"type":"string","description":"Exact skill name from the catalog"}},"required":["name"]})
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::compaction::{compaction_prompt_text, COMPACTION_SUMMARY_PREAMBLE};
    use crate::types::{FunctionCall, ToolCall};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn hermetic_cwd() -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("atom-ctx-cwd-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn ctx_session() -> Session {
        Session {
            cwd: hermetic_cwd(),
            ..Default::default()
        }
    }

    fn sys_msg(content: &str) -> Message {
        Message {
            role: "system".into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn role_msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn row_by_name(rows: &[ContextRow], name: &str) -> ContextRow {
        rows.iter()
            .find(|r| r.name == name)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn classify_instruction_categories() {
        const ATOM: &str = CONTEXT_CAT_ATOM;
        const REPO: &str = CONTEXT_CAT_REPO;
        let cases = [
            ("Instructions from: TOOLS.md\nbundled tools", ATOM),
            ("Instructions from: skills\n- pack: Pack files", REPO),
            (
                "Instructions from: /Users/me/proj/AGENTS.md\nproject rules",
                REPO,
            ),
            (
                "Instructions from: /Users/me/.config/atom/AGENTS.md\nglobal",
                ATOM,
            ),
            (
                "Instructions from: /home/u/.config/atom/AGENTS.md\nglobal",
                ATOM,
            ),
            ("Instructions from: mystery\nunknown prefix", ATOM),
            ("no prefix at all", ATOM),
        ];
        for (content, want) in cases {
            assert_eq!(classify_instruction(content), want, "classify({content:?})");
        }
    }

    #[test]
    fn breakdown_instruction_buckets() {
        let mut sess = ctx_session();
        sess.instructions = vec![
            sys_msg(&format!("Instructions from: TOOLS.md\n{}", "T".repeat(400))),
            sys_msg(&format!("Instructions from: skills\n{}", "S".repeat(400))),
            sys_msg(&format!(
                "Instructions from: /tmp/proj/AGENTS.md\n{}",
                "P".repeat(400)
            )),
            sys_msg(&format!(
                "Instructions from: /home/u/.config/atom/AGENTS.md\n{}",
                "G".repeat(400)
            )),
        ];
        let rows = context_breakdown(&sess);
        assert_eq!(rows.len(), 5);
        let atom = row_by_name(&rows, CONTEXT_CAT_ATOM);
        let repo = row_by_name(&rows, CONTEXT_CAT_REPO);
        assert!(
            atom.tokens > 0,
            "TOOLS.md + config AGENTS.md should count as atom"
        );
        assert!(
            repo.tokens > 0,
            "skills + project AGENTS.md should count as repo"
        );
        let atom_instr = estimate_tokens(
            sess.instructions[0].content.len() + sess.instructions[3].content.len(),
        );
        let repo_instr = estimate_tokens(
            sess.instructions[1].content.len() + sess.instructions[2].content.len(),
        );
        assert_eq!(atom.tokens, atom_instr);
        assert_eq!(repo.tokens, repo_instr);
    }

    #[test]
    fn breakdown_user_agent_and_compaction() {
        let tc = ToolCall {
            id: String::new(),
            call_type: String::new(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
        };
        let mut sess = ctx_session();
        sess.compaction_summary = "folded older turns".into();
        sess.compacted_through = 2;
        sess.messages = vec![
            role_msg("user", "old user"),
            role_msg("assistant", "old assistant"),
            role_msg("user", &format!("live user {}", "u".repeat(80))),
            Message {
                role: "assistant".into(),
                content: "live assistant".into(),
                reasoning: "think".into(),
                tool_calls: vec![tc],
                ..Default::default()
            },
            Message {
                role: "tool".into(),
                content: format!("tool output {}", "t".repeat(80)),
                ..Default::default()
            },
            role_msg("nudge", "please continue"),
        ];
        let rows = context_breakdown(&sess);
        let user = row_by_name(&rows, CONTEXT_CAT_USER);
        let agent = row_by_name(&rows, CONTEXT_CAT_AGENT);
        assert!(
            user.tokens > 0,
            "user bucket should include preamble, live user, and nudge"
        );
        assert!(
            agent.tokens > 0,
            "agent bucket should include ack, assistant, and tool"
        );

        let live = llm_messages(&sess);
        let mut user_chars = 0usize;
        let mut agent_chars = 0usize;
        for m in &live {
            if m.role == "system" {
                continue;
            }
            let n = estimate_message_chars(m) as usize;
            if m.role == "user" {
                user_chars += n;
            } else {
                agent_chars += n;
            }
        }
        assert_eq!(user.tokens, estimate_tokens(user_chars));
        assert_eq!(agent.tokens, estimate_tokens(agent_chars));
        assert!(
            live.iter()
                .any(|m| m.role == "user" && m.content.contains(COMPACTION_SUMMARY_PREAMBLE)),
            "expected compaction preamble in llmMessages user role"
        );
    }

    #[test]
    fn breakdown_tools_and_percents() {
        let rows = context_breakdown(&ctx_session());
        assert_eq!(rows.len(), 5, "empty session rows");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        let sum: i64 = rows.iter().map(|r| r.pct).sum();
        assert_eq!(names, CONTEXT_CATEGORY_ORDER.to_vec());
        let tools = row_by_name(&rows, CONTEXT_CAT_TOOLS);
        assert!(
            tools.tokens > 0,
            "builtin tool definitions should be present"
        );
        assert_eq!(sum, 100);
    }

    #[test]
    fn empty_rows_zero_percent() {
        let got = allocate_percents(&[0, 0, 0, 0, 0]);
        assert_eq!(got.len(), 5);
        assert!(got.iter().all(|p| *p == 0));
        let rows = context_breakdown(&ctx_session());
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn allocate_percents_largest_remainder() {
        let got = allocate_percents(&[1, 1, 1]);
        assert_eq!(got.iter().sum::<i64>(), 100, "{got:?}");
        assert_eq!(got, vec![34, 33, 33]);
    }

    #[test]
    fn instruction_source_extraction() {
        assert_eq!(
            instruction_source("Instructions from: TOOLS.md\nbody"),
            "TOOLS.md"
        );
        assert_eq!(instruction_source("no prefix"), "");
        assert_eq!(
            instruction_source("Instructions from:   /x/y AGENTS.md  \nnext"),
            "/x/y AGENTS.md"
        );
        let rows = context_breakdown(&Session::default());
        assert!(row_by_name(&rows, CONTEXT_CAT_USER).pct >= 0);
        let _ = compaction_prompt_text("x"); // touch import for parity helpers
    }
}
