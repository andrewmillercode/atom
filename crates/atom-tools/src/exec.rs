//! ToolCtx + execute_tool, ported from main.go executeToolFor. The bash
//! tool routes through the atom-sandbox execution pipeline instead of a
//! bare `bash -c`, formatting results like Go's CombinedOutput handling.

use crate::dispatch::{self, DispatchPlan};
use crate::file_edit::FileSeen;
use crate::mcp;
use crate::{
    customize, file_edit, read_file, search, skills, vector_search, visualize, web_fetch,
    web_search,
};
use atom_core::cancel::CancelToken;
use atom_core::types::ImageData;
use atom_sandbox::approvals::Approver;
use atom_sandbox::policy::SandboxConfig;
use std::path::PathBuf;

/// empty_arguments_msg is the standard diagnostic when a tool call
/// arrives with no JSON arguments at all (an empty or whitespace-only
/// string). serde's own error in that case — "EOF while parsing a value
/// at line 1 column 0" — is technically correct but confuses models
/// into thinking the stream/network closed early, and they end up
/// retrying with the same broken call. This message keeps the
/// "error parsing arguments:" prefix so callers that grep on it keep
/// working, but spells out the cause and the fix so the next model turn
/// can recover.
pub(crate) fn empty_arguments_msg(tool: &str) -> String {
    format!(
        "error parsing arguments: model emitted no arguments for tool \"{tool}\" (empty arguments string); pass a JSON object like {{\"arg\": \"value\"}}"
    )
}

/// Everything a tool call needs: session identity, provider plumbing for
/// dispatch turns, sandbox policy, the approval gate, the subagent
/// spawner, the per-session seen-file cache, and the turn's
/// cancellation token (Esc / interruption kills in-flight commands).
pub struct ToolCtx<'a> {
    pub cwd: PathBuf,
    pub session_id: String,
    pub api_key: String,
    pub base_url: String,
    pub reasoning_field: String,
    pub sandbox_cfg: SandboxConfig,
    pub approver: &'a dyn Approver,
    /// Session-store + turn-loop bridge; None mirrors Go's nil store
    /// ("error: dispatch requires an active session").
    pub spawner: Option<&'a dyn SubagentHandle>,
    /// Per-session fileSeen cache (Go keeps it on Session). None means
    /// write_file/edit_file always report "not been read".
    pub file_seen: Option<&'a FileSeen>,
    /// Turn cancellation: a cancelled token makes in-flight bash return
    /// immediately (killing the command) instead of running to its
    /// timeout. None in tests.
    pub cancel: Option<&'a CancelToken>,
}

/// Resolve a model-supplied path against the session workspace. Tool code
/// must never let relative paths fall through to the server process cwd.
pub(crate) fn resolve_tool_path(cwd: &std::path::Path, path: &str) -> PathBuf {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Result of one tool call: model-visible text, optional image
/// attachments (read_file on an image file), plus a unified diff of any
/// file change ("" when the tool didn't change a file).
#[derive(Default, Clone, Debug)]
pub struct ToolOutcome {
    pub text: String,
    pub images: Vec<ImageData>,
    pub diff: String,
    /// Which provider served this call (search/fetch only): a bundled id
    /// ("tinyfish", "parallel", "exa", "ollama"), "direct" for the direct
    /// webfetch fallback, or "mcp:<server>" for user-configured servers.
    /// Empty for all other tools.
    pub tool_provider: String,
}

impl ToolOutcome {
    pub(crate) fn from_text(text: String) -> Self {
        ToolOutcome {
            text,
            ..Default::default()
        }
    }
}

/// The server-side dispatch bridge: creates child sessions, posts
/// follow-ups, cancels active turns, and returns Go-identical result
/// strings ("id: ...\nmodel: ...\nthinking: ...", "ok: continued\n...",
/// "ok: cancelled\nid: ...", or "error: ...").
#[async_trait::async_trait]
pub trait SubagentHandle: Send + Sync {
    async fn spawn(&self, plan: DispatchPlan) -> String;
    async fn cont(&self, plan: DispatchPlan) -> String;
    async fn cancel(&self, sid: &str) -> String;
    /// result returns a bulk status snapshot and optionally waits according
    /// to `plan.wait_mode`.
    async fn result(&self, plan: DispatchPlan) -> String;
}

/// executeTool runs a named tool call with JSON-encoded arguments.
pub async fn execute_tool(ctx: &ToolCtx<'_>, name: &str, args_json: &str) -> ToolOutcome {
    match name {
        "skill" => ToolOutcome::from_text(skills::execute_skill(
            args_json,
            &ctx.cwd.display().to_string(),
        )),
        "customize" => ToolOutcome::from_text(customize::execute_customize(args_json)),
        "dispatch" => ToolOutcome::from_text(dispatch::execute_dispatch(ctx, args_json).await),
        "web_search" => {
            #[derive(serde::Deserialize)]
            struct Args {
                #[serde(default)]
                query: String,
            }
            if args_json.trim().is_empty() {
                return ToolOutcome::from_text(empty_arguments_msg("web_search"));
            }
            let args: Args = match serde_json::from_str(args_json) {
                Ok(a) => a,
                Err(e) => return ToolOutcome::from_text(format!("error parsing arguments: {e}")),
            };
            web_search::web_search(&args.query, &ctx.cwd).await
        }
        "webfetch" => web_fetch::web_fetch(args_json, ctx).await,
        "vector_search" => {
            ToolOutcome::from_text(vector_search::vector_search(args_json, &ctx.cwd).await)
        }
        "grep" => ToolOutcome::from_text(search::grep_search(args_json, &ctx.cwd).await),
        "glob" => ToolOutcome::from_text(search::glob_search(args_json, &ctx.cwd).await),
        "read_file" => read_file::execute_read_file(args_json, ctx),
        "find_tool" => ToolOutcome::from_text(mcp::execute_find_tool(args_json, &ctx.cwd).await),
        "visualize" => visualize::execute_visualize(args_json, ctx).await,
        "write_file" => file_edit::execute_write_file(args_json, ctx).await,
        "edit_file" => file_edit::execute_edit_file(args_json, ctx).await,
        "bash" => execute_bash(args_json, ctx).await,
        "jobs" => execute_jobs(args_json, ctx).await,
        _ => {
            if name.starts_with("mcp_") {
                return ToolOutcome::from_text(
                    mcp::execute_mcp_tool(name, args_json, &ctx.cwd).await,
                );
            }
            if mcp::hub_lookup(name, &ctx.cwd).await {
                return ToolOutcome::from_text(
                    mcp::execute_mcp_tool(name, args_json, &ctx.cwd).await,
                );
            }
            ToolOutcome::from_text(format!("unknown tool: {name}"))
        }
    }
}

/// Bash via the sandbox pipeline. Background by default: the result
/// comes back inline when the command finishes quickly (a ~10s head
/// start — cut short by a mid-turn prompt), otherwise the call returns
/// the job id immediately and the command keeps running; an explicit
/// `background: false` blocks up to the sandbox timeout. Output shapes
/// mirror Go's CombinedOutput handling:
/// - success: TrimSpace(stdout+stderr)
/// - non-zero exit: "exit status N\n<combined output>"
/// - policy refusal / not approved: the sandbox's explanatory stderr
/// - timeout / cancel / job handoff: an explanatory line.
async fn execute_bash(args_json: &str, ctx: &ToolCtx<'_>) -> ToolOutcome {
    #[derive(serde::Deserialize)]
    struct Args {
        #[serde(default)]
        command: String,
        /// None (default): background by default — inline result when
        /// quick, otherwise the job id. false: block until exit (or
        /// the sandbox timeout). true: job id immediately, no wait.
        #[serde(default)]
        background: Option<bool>,
    }
    if args_json.trim().is_empty() {
        return ToolOutcome::from_text(empty_arguments_msg("bash"));
    }
    let args: Args = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::from_text(format!("error parsing arguments: {e}")),
    };
    let mode = match args.background {
        None => atom_sandbox::exec::BashMode::Background,
        Some(false) => atom_sandbox::exec::BashMode::Block,
        Some(true) => atom_sandbox::exec::BashMode::Detach,
    };
    let out = atom_sandbox::exec::run_tool(
        &args.command,
        &ctx.cwd,
        &ctx.cwd,
        &ctx.session_id,
        &ctx.sandbox_cfg,
        ctx.approver,
        mode,
        ctx.cancel,
    )
    .await;

    if let Some(job_id) = out.backgrounded {
        let mut text = format!(
            "command still running as job {job_id}. \
             Check progress with the jobs tool (action=check), block for completion \
             with action=wait, or stop it with action=kill."
        );
        let partial = out.stdout.trim();
        if !partial.is_empty() {
            text.push_str("\noutput so far:\n");
            text.push_str(partial);
        }
        return ToolOutcome::from_text(text);
    }
    if out.cancelled {
        let mut text = "error: command cancelled by the user".to_string();
        let partial = out.stdout.trim();
        if !partial.is_empty() {
            text.push_str("\npartial output:\n");
            text.push_str(partial);
        }
        return ToolOutcome::from_text(text);
    }
    if out.timed_out {
        return ToolOutcome::from_text(format!(
            "error: command timed out after {}s",
            atom_sandbox::exec::EXEC_TIMEOUT.as_secs()
        ));
    }
    if out.exit_code < 0 && !out.stderr.is_empty() && !out.approved {
        // Blocked before running (deny verdict / refused approval /
        // spawn failure): surface the sandbox's message verbatim.
        return ToolOutcome::from_text(out.stderr.trim().to_string());
    }
    let combined = format!("{}{}", out.stdout, out.stderr);
    if out.exit_code != 0 {
        return ToolOutcome::from_text(format!("exit status {}\n{}", out.exit_code, combined));
    }
    ToolOutcome::from_text(combined.trim().to_string())
}

/// The `jobs` tool: inspect, wait for, or kill background commands
/// started by the bash tool. Results belong to the calling session.
async fn execute_jobs(args_json: &str, ctx: &ToolCtx<'_>) -> ToolOutcome {
    #[derive(serde::Deserialize)]
    struct Args {
        #[serde(default)]
        action: Option<String>,
        #[serde(default)]
        ids: Vec<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    }
    if args_json.trim().is_empty() {
        return ToolOutcome::from_text(empty_arguments_msg("jobs"));
    }
    let args: Args = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::from_text(format!("error parsing arguments: {e}")),
    };
    let action = args.action.as_deref().unwrap_or("check");
    let lines = match action {
        "check" => atom_sandbox::jobs::check(&ctx.session_id, &args.ids),
        "wait" => {
            let timeout = std::time::Duration::from_millis(
                args.timeout_ms
                    .unwrap_or(atom_sandbox::jobs::WAIT_TIMEOUT.as_millis() as u64),
            );
            atom_sandbox::jobs::wait(&ctx.session_id, &args.ids, timeout).await
        }
        "kill" => atom_sandbox::jobs::kill(&ctx.session_id, &args.ids).await,
        _other => {
            return ToolOutcome::from_text(format!(
                "error: action must be one of check, wait, kill (got \"{action}\")"
            ))
        }
    };
    if lines.is_empty() {
        return ToolOutcome::from_text(
            "no background jobs (start one with the bash tool; long commands become jobs automatically)"
                .into(),
        );
    }
    ToolOutcome::from_text(lines.join("\n\n"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use atom_sandbox::approvals::{AutoApprover, Decision};
    use once_cell::sync::Lazy;

    static ALLOW_ONCE: Lazy<AutoApprover> = Lazy::new(|| AutoApprover(Decision::AllowOnce));

    pub fn off_cfg() -> SandboxConfig {
        SandboxConfig::default()
    }

    /// A ctx with no seen-file cache, Off sandbox, auto-approving.
    pub fn test_ctx(ws: &std::path::Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: ws.to_path_buf(),
            session_id: "test-session".to_string(),
            api_key: String::new(),
            base_url: String::new(),
            reasoning_field: String::new(),
            sandbox_cfg: off_cfg(),
            approver: &*ALLOW_ONCE,
            spawner: None,
            file_seen: None,
            cancel: None,
        }
    }

    /// A ctx wired to a dispatcher (subagent orchestration tests).
    pub fn ctx_with_spawner<'a>(
        ws: &std::path::Path,
        spawner: &'a dyn SubagentHandle,
    ) -> ToolCtx<'a> {
        ToolCtx {
            cwd: ws.to_path_buf(),
            session_id: "test-session".to_string(),
            api_key: String::new(),
            base_url: String::new(),
            reasoning_field: String::new(),
            sandbox_cfg: off_cfg(),
            approver: &*ALLOW_ONCE,
            spawner: Some(spawner),
            file_seen: None,
            cancel: None,
        }
    }

    /// Env with workspace tempdir + FileSeen cache for edit/write flows.
    pub struct FileEnv {
        pub ws: tempfile::TempDir,
        pub seen: FileSeen,
    }

    impl FileEnv {
        pub fn new() -> Self {
            FileEnv {
                ws: tempfile::tempdir().unwrap(),
                seen: FileSeen::new(),
            }
        }

        pub fn ctx(&self) -> ToolCtx<'_> {
            self.ctx_with(&*ALLOW_ONCE)
        }

        pub fn ctx_with<'a>(&'a self, approver: &'a dyn Approver) -> ToolCtx<'a> {
            ToolCtx {
                cwd: self.ws.path().to_path_buf(),
                session_id: "test-session".to_string(),
                api_key: String::new(),
                base_url: String::new(),
                reasoning_field: String::new(),
                sandbox_cfg: off_cfg(),
                approver,
                spawner: None,
                file_seen: Some(&self.seen),
                cancel: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::test_support::*;

    #[tokio::test]
    async fn bash_success_trims_combined_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "bash", r#"{"command":"echo hi"}"#).await;
        assert_eq!(out.text, "hi");
        assert!(out.images.is_empty() && out.diff.is_empty());
    }

    #[tokio::test]
    async fn bash_nonzero_exit_reports_status_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "bash", r#"{"command":"echo boom; exit 3"}"#).await;
        assert!(out.text.starts_with("exit status 3\n"), "{}", out.text);
        assert!(out.text.contains("boom"), "{}", out.text);
    }

    #[tokio::test]
    async fn bash_bad_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "bash", "{not json").await;
        assert!(
            out.text.starts_with("error parsing arguments:"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn bash_empty_arguments_friendly_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "bash", "").await;
        assert!(
            out.text.starts_with("error parsing arguments:"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("\"bash\""),
            "tool name should appear in the message: {}",
            out.text
        );
        assert!(
            out.text.contains("empty arguments string"),
            "reason should be explained: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn read_file_empty_arguments_friendly_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "read_file", "   ").await;
        assert!(
            out.text.starts_with("error parsing arguments:"),
            "{}",
            out.text
        );
        assert!(out.text.contains("\"read_file\""), "{}", out.text);
    }

    #[test]
    fn empty_arguments_msg_format() {
        let m = empty_arguments_msg("grep");
        assert!(m.starts_with("error parsing arguments:"));
        assert!(m.contains("\"grep\""));
        assert!(m.contains("empty arguments string"));
        assert!(m.contains("JSON object"));
    }

    #[tokio::test]
    async fn unknown_tool_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "teleport", "{}").await;
        assert_eq!(out.text, "unknown tool: teleport");
    }

    #[tokio::test]
    async fn read_and_write_via_execute_tool() {
        let env = FileEnv::new();
        let path = env.ws.path().join("x.txt");
        let pjson = serde_json::json!({"path": path.display().to_string()});

        let ctx = env.ctx_with(&atom_sandbox::approvals::AutoApprover(
            atom_sandbox::approvals::Decision::AllowOnce,
        ));
        // Write to a not-yet-seen existing file errors first.
        std::fs::write(&path, "old\n").unwrap();
        let out = execute_tool(
            &ctx,
            "write_file",
            &serde_json::json!({"path": path.display().to_string(), "content": "new\n"})
                .to_string(),
        )
        .await;
        assert!(
            out.text.starts_with("error: file has not been read"),
            "{}",
            out.text
        );

        // Reading registers it.
        let out = execute_tool(&ctx, "read_file", &pjson.to_string()).await;
        assert_eq!(out.text, "old\n");

        // Now the write applies with a diff.
        let out = execute_tool(
            &ctx,
            "write_file",
            &serde_json::json!({"path": path.display().to_string(), "content": "new\n"})
                .to_string(),
        )
        .await;
        assert!(out.text.starts_with("wrote 4 bytes to "), "{}", out.text);
        assert!(!out.diff.is_empty());
        let _ = pjson;
    }

    #[tokio::test]
    async fn relative_file_tools_resolve_from_session_cwd() {
        let env = FileEnv::new();
        let path = env.ws.path().join("relative.txt");
        std::fs::write(&path, "old\n").unwrap();
        let ctx = env.ctx_with(&atom_sandbox::approvals::AutoApprover(
            atom_sandbox::approvals::Decision::AllowOnce,
        ));

        let read = execute_tool(&ctx, "read_file", r#"{"path":"relative.txt"}"#).await;
        assert_eq!(read.text, "old\n");
        let edit = execute_tool(
            &ctx,
            "edit_file",
            r#"{"path":"relative.txt","old_text":"old","new_text":"new"}"#,
        )
        .await;
        assert!(
            edit.text.starts_with("edited relative.txt"),
            "{}",
            edit.text
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn skill_unknown_lists_nothing_without_catalog() {
        let dir = tempfile::tempdir().unwrap(); // empty cwd → no project skills
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "skill", r#"{"name":"nope"}"#).await;
        // Prefix check: user-level skill catalogs (~/.agents/skills etc.)
        // legitimately exist on dev machines, so the "(known: …)" suffix
        // may be non-empty — but the unknown-name error must always lead.
        assert!(
            out.text.starts_with("error: unknown skill \"nope\""),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn dispatch_requires_spawner() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"spawn","model":"m","thinking":"low","tasks":["x"]}"#,
        )
        .await;
        assert_eq!(out.text, "error: dispatch requires an active session");
    }

    struct FakeSpawner {
        last: std::sync::Mutex<String>,
        spawned: std::sync::Mutex<Vec<DispatchPlan>>,
    }

    impl FakeSpawner {
        fn new() -> Self {
            FakeSpawner {
                last: std::sync::Mutex::new(String::new()),
                spawned: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn called(&self) -> String {
            self.last.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SubagentHandle for FakeSpawner {
        async fn spawn(&self, plan: DispatchPlan) -> String {
            *self.last.lock().unwrap() = "spawn".into();
            self.spawned.lock().unwrap().push(plan.clone());
            serde_json::json!({"id":"0123456789abcdef","model":plan.model,"thinking":plan.thinking})
                .to_string()
        }
        async fn cont(&self, plan: DispatchPlan) -> String {
            *self.last.lock().unwrap() = "cont".into();
            format!("ok: continued\nid: {}", plan.session_id)
        }
        async fn cancel(&self, sid: &str) -> String {
            *self.last.lock().unwrap() = format!("cancel:{sid}");
            format!("ok: cancelled\nid: {sid}")
        }
        async fn result(&self, plan: DispatchPlan) -> String {
            *self.last.lock().unwrap() = format!("result:{}", plan.wait_mode);
            let ids = if plan.ids.is_empty() {
                vec!["0123456789abcdef".to_string()]
            } else {
                plan.ids
            };
            serde_json::json!({
                "batch_id": plan.batch_id,
                "counts": {"done": ids.len()},
                "delegates": ids.into_iter().map(|id| serde_json::json!({"id":id,"status":"done","result":"hello"})).collect::<Vec<_>>()
            }).to_string()
        }
    }

    fn spawner_ctx(spawner: &FakeSpawner) -> ToolCtx<'_> {
        crate::exec::test_support::ctx_with_spawner(std::path::Path::new("/tmp/ws"), spawner)
    }

    #[tokio::test]
    async fn dispatch_routes_inspect_send_cancel() {
        let s = FakeSpawner::new();

        let ctx = spawner_ctx(&s);
        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"spawn","model":"m","thinking":"low","tasks":["hi"]}"#,
        )
        .await;
        assert!(out.text.contains("\"delegates\""), "{}", out.text);
        assert_eq!(s.spawned.lock().unwrap().len(), 1);

        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"send","ids":["0123456789abcdef"],"prompt":"go on"}"#,
        )
        .await;
        assert!(out.text.contains("\"status\":\"done\""), "{}", out.text);

        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"inspect","ids":["0123456789abcdef"]}"#,
        )
        .await;
        assert!(out.text.contains("\"status\":\"done\""), "{}", out.text);

        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"inspect","ids":["0123456789abcdef"],"wait":"all"}"#,
        )
        .await;
        assert!(out.text.contains("\"result\":\"hello\""), "{}", out.text);
        assert_eq!(s.called(), "result:all");

        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"cancel","ids":["0123456789abcdef"]}"#,
        )
        .await;
        assert!(out.text.contains("\"delegates\""), "{}", out.text);
    }

    #[tokio::test]
    async fn dispatch_batch_spawns_every_task_with_shared_defaults() {
        let s = FakeSpawner::new();
        let ctx = spawner_ctx(&s);
        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"spawn","provider":"shared-provider","model":"shared","thinking":"high","tasks":["one","two"]}"#,
        )
        .await;

        assert!(out.text.contains("\"batch_id\""));
        let spawned = s.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 2);
        assert_eq!(spawned[0].provider, "shared-provider");
        assert_eq!(spawned[0].model, "shared");
        assert_eq!(spawned[0].thinking, "high");
        assert_eq!(spawned[0].prompt, "one");
        assert_eq!(spawned[1].provider, "shared-provider");
        assert_eq!(spawned[1].model, "shared");
        assert_eq!(spawned[1].thinking, "high");
        assert_eq!(spawned[1].prompt, "two");
        assert_eq!(spawned[0].batch_id, spawned[1].batch_id);
        assert_eq!((spawned[0].batch_index, spawned[1].batch_index), (1, 2));
    }

    #[tokio::test]
    async fn dispatch_spawn_rejects_invalid_tasks() {
        let s = FakeSpawner::new();
        let ctx = spawner_ctx(&s);
        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"spawn","thinking":"high","tasks":[]}"#,
        )
        .await;
        assert_eq!(out.text, "error: spawn requires at least one task");

        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"spawn","thinking":"high","tasks":["  "]}"#,
        )
        .await;
        assert_eq!(out.text, "error: every task must be a non-empty string");
        assert!(s.spawned.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_spawn_accepts_single_string_task() {
        let s = FakeSpawner::new();
        let ctx = spawner_ctx(&s);
        let out = execute_tool(
            &ctx,
            "dispatch",
            r#"{"action":"spawn","thinking":"high","tasks":"just one task"}"#,
        )
        .await;
        assert!(!out.text.starts_with("error"));
        let spawned = s.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].prompt, "just one task");
    }

    #[tokio::test]
    async fn dispatch_inspect_without_target_lists_all() {
        let s = FakeSpawner::new();
        let ctx = spawner_ctx(&s);
        let out = execute_tool(&ctx, "dispatch", r#"{"action":"inspect"}"#).await;
        assert!(out.text.contains("\"delegates\""));
    }

    // ---- background bash + jobs tool ----

    /// Isolates the atom data dir for one test (XDG_DATA_HOME is
    /// process-global, so a mutex guards concurrent runs; background
    /// job logs land under dataDir()/jobs/<session>/).
    struct DataDirEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_xdg: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl DataDirEnv {
        fn new() -> Self {
            static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::TempDir::new().unwrap();
            let prev_xdg = std::env::var_os("XDG_DATA_HOME");
            std::env::set_var("XDG_DATA_HOME", dir.path());
            DataDirEnv {
                _lock: lock,
                prev_xdg,
                _dir: dir,
            }
        }
    }

    impl Drop for DataDirEnv {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    fn job_id_from(text: &str) -> String {
        let rest = &text[text.find("job ").expect("job id in result") + 4..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '.')
            .unwrap_or(rest.len());
        rest[..end].to_string()
    }

    #[tokio::test]
    async fn bash_background_true_returns_job_id_and_jobs_tool_manages_it() {
        let _env = DataDirEnv::new();
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());

        let out = execute_tool(&ctx, "bash", r#"{"command":"sleep 30","background":true}"#).await;
        assert!(
            out.text.starts_with("command still running"),
            "background ack expected: {}",
            out.text
        );
        let job_id = job_id_from(&out.text);

        // jobs check: the job exists and reports running.
        let out = execute_tool(&ctx, "jobs", r#"{"action":"check"}"#).await;
        assert!(out.text.contains(&job_id), "{}", out.text);
        assert!(out.text.contains("running"), "{}", out.text);

        // jobs kill: stops it; a follow-up check reports the exit.
        let out = execute_tool(
            &ctx,
            "jobs",
            &serde_json::json!({"action": "kill", "ids": [job_id]}).to_string(),
        )
        .await;
        assert!(out.text.contains("killed"), "{}", out.text);
        let out = execute_tool(
            &ctx,
            "jobs",
            &serde_json::json!({"action": "check", "ids": [job_id]}).to_string(),
        )
        .await;
        assert!(out.text.contains("exited"), "{}", out.text);
    }

    #[tokio::test]
    async fn bash_background_false_waits_for_exit() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(
            &ctx,
            "bash",
            r#"{"command":"echo sync","background":false}"#,
        )
        .await;
        assert_eq!(out.text, "sync");
    }

    #[tokio::test]
    async fn jobs_bad_action_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_tool(&ctx, "jobs", r#"{"action":"explode"}"#).await;
        assert!(
            out.text
                .starts_with("error: action must be one of check, wait, kill"),
            "{}",
            out.text
        );
    }

    #[test]
    fn tool_outcome_default_is_empty() {
        let o: ToolOutcome = Default::default();
        assert!(o.text.is_empty() && o.images.is_empty() && o.diff.is_empty());
    }
}
