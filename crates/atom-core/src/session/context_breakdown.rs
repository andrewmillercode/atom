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
    n.div_ceil(4) as i64
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
    // Bundled atom instructions (instructions/...).
    if src.starts_with("instructions/") {
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
/// The caller passes the tool definitions to count: the builtins live in
/// atom-tools (`atom_tools::tool_definitions`, plus MCP tools for cwd
/// in Go's toolDefinitionsFor(cwd)), and atom-core cannot depend on
/// atom-tools without a cycle.
pub fn context_breakdown(sess: &Session, tools: &[ToolDef]) -> Vec<ContextRow> {
    let mut chars: std::collections::HashMap<&'static str, usize> =
        CONTEXT_CATEGORY_ORDER.iter().map(|&c| (c, 0)).collect();

    for m in &sess.instructions {
        *chars.entry(classify_instruction(&m.content)).or_insert(0) += m.content.len();
    }

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

    /// A stand-in tool list, since the defs live in atom-tools and this
    /// crate cannot depend on it.
    fn some_tools() -> Vec<ToolDef> {
        vec![ToolDef::new(
            "bash",
            "Run commands that require a shell, such as tests, builds, git, and package managers.",
            serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        )]
    }

    #[test]
    fn classify_instruction_categories() {
        const ATOM: &str = CONTEXT_CAT_ATOM;
        const REPO: &str = CONTEXT_CAT_REPO;
        let cases = [
            (
                "Instructions from: instructions/system-prompt.md\nbundled prompt",
                ATOM,
            ),
            (
                "Instructions from: instructions/notes.md\nbundled extra",
                ATOM,
            ),
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
            sys_msg(&format!(
                "Instructions from: instructions/notes.md\n{}",
                "T".repeat(400)
            )),
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
        let rows = context_breakdown(&sess, &[]);
        assert_eq!(rows.len(), 5);
        let atom = row_by_name(&rows, CONTEXT_CAT_ATOM);
        let repo = row_by_name(&rows, CONTEXT_CAT_REPO);
        assert!(
            atom.tokens > 0,
            "instructions/notes.md + config AGENTS.md should count as atom"
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
        let rows = context_breakdown(&sess, &[]);
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
        let rows = context_breakdown(&ctx_session(), &some_tools());
        assert_eq!(rows.len(), 5, "empty session rows");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        let sum: i64 = rows.iter().map(|r| r.pct).sum();
        assert_eq!(names, CONTEXT_CATEGORY_ORDER.to_vec());
        let tools = row_by_name(&rows, CONTEXT_CAT_TOOLS);
        assert!(
            tools.tokens > 0,
            "tool definitions passed by the caller should be counted"
        );
        assert_eq!(sum, 100);
    }

    #[test]
    fn empty_rows_zero_percent() {
        let got = allocate_percents(&[0, 0, 0, 0, 0]);
        assert_eq!(got.len(), 5);
        assert!(got.iter().all(|p| *p == 0));
        let rows = context_breakdown(&ctx_session(), &[]);
        assert_eq!(rows.len(), 5);
        let tools = row_by_name(&rows, CONTEXT_CAT_TOOLS);
        // An empty slice still serializes as "[]" (2 chars -> 1 token);
        // it must stay negligible compared with the builtin list.
        assert!(tools.tokens <= 1);
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
            instruction_source("Instructions from: instructions/notes.md\nbody"),
            "instructions/notes.md"
        );
        assert_eq!(instruction_source("no prefix"), "");
        assert_eq!(
            instruction_source("Instructions from:   /x/y AGENTS.md  \nnext"),
            "/x/y AGENTS.md"
        );
        let rows = context_breakdown(&Session::default(), &[]);
        assert!(row_by_name(&rows, CONTEXT_CAT_USER).pct >= 0);
        let _ = compaction_prompt_text("x"); // touch import for parity helpers
    }
}
