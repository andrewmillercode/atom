//! LLM reviewer for the approval gate.
//!
//! Unflagged Tier-2 commands get a fast model verdict instead of a
//! prompt: ACCEPT runs the command, while DENY, error, or timeout
//! falls back to the human prompt. The reviewer sees the command as
//! typed, the segments that execute after wrapper unwrapping, the cwd,
//! and static-analysis notes — never conversation, file contents, or
//! tool output, so injected content has no channel into the verdict.
//!
//! Flagged guardrails (`rm -rf`, `sudo`, …) always prompt. Every
//! verdict is appended to dataDir()/sandbox-reviewer.log and forwarded
//! to the session stream as an `auto_review` event, so the TUI can show
//! what the reviewer did.

use async_trait::async_trait;
use atom_core::config::ReviewerConfig;
use atom_core::providers::{
    anthropic_style_for_url, api_protocol_for, bedrock_style_for_url, provider_name_for_url,
    stream_anthropic, stream_bedrock, stream_chat, stream_responses, APIProtocol,
};
use atom_core::types::{ChatRequest, Message, StreamChunk};
use atom_core::util::sha256_hash;
use atom_sandbox::approvals::{ApprovalRequest, Approver, Decision};
use chrono::SecondsFormat;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fixed review policy — the model sees the command, what executes,
/// and what it touches. The user's steering instructions from config
/// are appended to this.
const SYSTEM_PROMPT: &str = "You review shell commands for a coding agent. You see the command as typed, the resolved segments that actually execute after shell-wrapper unwrapping, the cwd, and static-analysis notes on what the command touches. Reply exactly ACCEPT if this is a routine, local, reversible development action; otherwise reply DENY with a short reason, in the form \"DENY: reason\". When unsure, DENY.";

/// Wall-clock limit per review call. On timeout the human prompt runs.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(6);

/// Sampling for the one-token verdict.
const TEMPERATURE: f64 = 0.0;

/// The reviewer seam: a request body, a reply, or an error string.
#[async_trait]
pub trait ReviewClient: Send + Sync {
    async fn review(&self, user_content: &str) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// Empty reason when the model replied bare DENY.
    Deny(String),
    Error(String),
}

impl Verdict {
    fn as_str(&self) -> &'static str {
        match self {
            Verdict::Accept => "accept",
            Verdict::Deny(_) => "deny",
            Verdict::Error(_) => "error",
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Verdict::Error(e) => Some(e),
            _ => None,
        }
    }

    fn deny_reason(&self) -> Option<&str> {
        match self {
            Verdict::Deny(r) if !r.is_empty() => Some(r),
            _ => None,
        }
    }
}

/// Whole-reply parse: ACCEPT alone, or DENY optionally followed by a
/// reason after a separator. Anything else is an error, never an
/// accept.
fn parse_verdict(text: &str) -> Verdict {
    let s = text.trim().trim_end_matches('.');
    if s.eq_ignore_ascii_case("ACCEPT") {
        return Verdict::Accept;
    }
    let (head, rest) = s.split_at(s.len().min(4));
    if head.eq_ignore_ascii_case("DENY") {
        let reason = rest.trim_start_matches([':', '-', ' ']).trim();
        return Verdict::Deny(reason.to_string());
    }
    Verdict::Error(format!("unparseable verdict: {s:?}"))
}

/// The review request body: command as typed, the segments that
/// execute, and what static analysis saw in them. Uses the request's
/// workspace root (falling back to cwd for callers that predate the
/// field). Never includes conversation, file contents, or tool
/// output — the reviewer must stay unreachable by prompt injection.
fn review_context(req: &ApprovalRequest) -> String {
    let analysis = atom_sandbox::rules::analyze(&req.command, req.workspace());
    let mut out = format!("command: {}\ncwd: {}", req.command, req.cwd.display());
    if !analysis.segments.is_empty() {
        out.push_str("\nresolved segments (what executes):");
        for seg in &analysis.segments {
            out.push_str(&format!("\n- {}", seg.join(" ")));
        }
    }
    let mut facts: Vec<&str> = Vec::new();
    if analysis.uses_network {
        facts.push("network egress");
    }
    if analysis.paths_outside_workspace {
        facts.push("paths outside the workspace");
    }
    if analysis.touches_home {
        facts.push("paths under $HOME");
    }
    if analysis.writes_git_hooks {
        facts.push("writes to .git/hooks");
    }
    if !facts.is_empty() {
        out.push_str(&format!("\nstatic analysis: {}", facts.join("; ")));
    }
    out
}

/// Fold the user's plain-English steering into the policy prompt.
/// Steering biases the verdict; it never bypasses the review.
fn system_prompt(cfg: &ReviewerConfig) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();
    for (label, instructions) in [
        ("Lean toward accepting", &cfg.allow_instructions),
        ("Lean toward denying", &cfg.block_instructions),
    ] {
        if instructions.is_empty() {
            continue;
        }
        prompt.push_str(&format!("\n{label}:"));
        for instruction in instructions {
            prompt.push_str(&format!("\n- {instruction}"));
        }
    }
    prompt
}

/// Live seam: routes the review call through the same stream helpers
/// the turn loop uses, matching the session provider's wire dialect.
pub struct LiveReviewer {
    pub base_url: String,
    pub api_key: String,
    pub reasoning_field: String,
    /// Reasoning effort for the review call, "low" unless overridden.
    pub reasoning_effort: String,
    pub model: String,
    /// Policy prompt plus the user's steering instructions.
    pub system_prompt: String,
}

#[async_trait]
impl ReviewClient for LiveReviewer {
    async fn review(&self, user_content: &str) -> Result<String, String> {
        let msgs = vec![
            Message {
                role: "system".into(),
                content: self.system_prompt.clone(),
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: user_content.into(),
                ..Default::default()
            },
        ];
        if bedrock_style_for_url(&self.base_url) {
            collect(stream_bedrock(
                &self.base_url,
                &self.api_key,
                &self.model,
                &msgs,
                &[],
                &self.reasoning_effort,
            ))
            .await
        } else if anthropic_style_for_url(&self.base_url) {
            collect(stream_anthropic(
                &self.base_url,
                &self.api_key,
                &self.model,
                &msgs,
                &[],
                &self.reasoning_effort,
            ))
            .await
        } else if api_protocol_for(&provider_name_for_url(&self.base_url), &self.model)
            == APIProtocol::OpenAIResponses
        {
            collect(stream_responses(
                &self.base_url,
                &self.api_key,
                &self.model,
                &msgs,
                &[],
                &self.reasoning_effort,
            ))
            .await
        } else {
            let req = ChatRequest {
                model: self.model.clone(),
                messages: msgs,
                stream: true,
                tools: vec![],
                reasoning_effort: self.reasoning_effort.clone(),
                stream_options: None,
                temperature: Some(TEMPERATURE),
                max_tokens: None,
            };
            collect(stream_chat(
                &self.base_url,
                &self.api_key,
                req,
                &self.reasoning_field,
            ))
            .await
        }
    }
}

/// Concatenate the stream's content deltas — the whole reply.
async fn collect<S, F>(stream_fut: F) -> Result<String, String>
where
    S: futures::Stream<Item = anyhow::Result<StreamChunk>>,
    F: std::future::Future<Output = anyhow::Result<S>>,
{
    let stream = match stream_fut.await {
        Ok(s) => s,
        Err(e) => return Err(e.to_string()),
    };
    let mut text = String::new();
    futures::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                for choice in &c.choices {
                    text.push_str(&choice.delta.content);
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(text)
}

/// Cache shared by the reviewer instances of one turn, so a repeated
/// command doesn't re-bill: (session_id, command) -> verdict.
pub type VerdictCache = Arc<Mutex<HashMap<(String, String), Verdict>>>;

/// Human approver with the reviewer in front of it.
pub struct ReviewerApprover<'a> {
    inner: &'a dyn Approver,
    client: &'a dyn ReviewClient,
    cfg: ReviewerConfig,
    /// Effective model for the review call and the log's `model` field.
    model: String,
    log_dir: PathBuf,
    cache: VerdictCache,
    /// Set when the wiring site wants verdicts on the session stream.
    emit: Option<EventSink>,
}

/// Forwards an `auto_review` event to the session's subscribers.
pub type EventSink = Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// One turn's reviewer: config, model and endpoint resolved once and
/// shared by the decorator each tool call builds.
pub struct TurnReviewer {
    cfg: ReviewerConfig,
    model: String,
    client: LiveReviewer,
    log_dir: PathBuf,
    emit: EventSink,
}

impl TurnReviewer {
    /// Resolve the reviewer for a turn. It runs on the session's own
    /// model unless `/settings` picked another provider/model, and its
    /// reasoning is floored at `low`.
    pub async fn new(
        session_model: &str,
        base_url: &str,
        api_key: &str,
        reasoning_field: &str,
        emit: EventSink,
    ) -> Self {
        let cfg = atom_core::config::load().resolved_reviewer();
        atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
        let (model, endpoint) = if cfg.has_model_override() {
            let provider =
                atom_core::providers::providers::resolve_provider_endpoint(&cfg.provider).await;
            (cfg.model.clone(), provider)
        } else {
            (
                session_model.to_string(),
                atom_core::providers::providers::Provider {
                    name: atom_core::providers::providers::provider_name_for_url(base_url),
                    base_url: base_url.to_string(),
                    key: api_key.to_string(),
                    reasoning_field: reasoning_field.to_string(),
                    ..Default::default()
                },
            )
        };
        let levels = atom_core::providers::modelsdev::reasoning_levels_for(&endpoint.name, &model)
            .unwrap_or_default();
        let client = LiveReviewer {
            base_url: endpoint.base_url,
            api_key: endpoint.key,
            reasoning_field: endpoint.reasoning_field,
            reasoning_effort: atom_core::config::clamp_reasoning(
                &cfg.requested_reasoning(),
                &levels,
            ),
            model: model.clone(),
            system_prompt: system_prompt(&cfg),
        };
        TurnReviewer {
            cfg,
            model,
            client,
            log_dir: atom_core::session::store::data_dir(),
            emit,
        }
    }

    /// The decorator for one tool call, or `None` when the reviewer is
    /// off (the caller then uses the human approver directly).
    pub fn wrap<'a>(
        &'a self,
        human: &'a dyn Approver,
        cache: VerdictCache,
    ) -> Option<ReviewerApprover<'a>> {
        if !self.cfg.resolved_enabled() {
            return None;
        }
        Some(ReviewerApprover::new(
            human,
            &self.client,
            self.cfg.clone(),
            self.model.clone(),
            self.log_dir.clone(),
            cache,
            Some(self.emit.clone()),
        ))
    }
}

impl<'a> ReviewerApprover<'a> {
    pub fn new(
        inner: &'a dyn Approver,
        client: &'a dyn ReviewClient,
        cfg: ReviewerConfig,
        model: String,
        log_dir: PathBuf,
        cache: VerdictCache,
        emit: Option<EventSink>,
    ) -> Self {
        ReviewerApprover {
            inner,
            client,
            cfg,
            model,
            log_dir,
            cache,
            emit,
        }
    }

    /// Whether the reviewer is on — the caller picks this decorator or
    /// the bare human approver.
    pub fn enabled(&self) -> bool {
        self.cfg.resolved_enabled()
    }

    /// Best-effort JSONL append, like the audit writer.
    fn log(&self, req: &ApprovalRequest, verdict: &Verdict, latency_ms: u128) {
        let mut record = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
            "session_id": req.session_id,
            "cmd_sha256": sha256_hash(req.command.as_bytes()),
            "verdict": verdict.as_str(),
            "model": self.model,
            "latency_ms": latency_ms as u64,
        });
        if let Some(e) = verdict.error() {
            record["error"] = serde_json::json!(e);
        }
        if let Some(r) = verdict.deny_reason() {
            record["reason"] = serde_json::json!(r);
        }
        if std::fs::create_dir_all(&self.log_dir).is_err() {
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_dir.join("sandbox-reviewer.log"))
        {
            use std::io::Write;
            let _ = writeln!(f, "{record}");
        }
    }

    /// Forward the verdict to the session stream so the TUI can show it.
    fn emit(&self, req: &ApprovalRequest, verdict: &Verdict, latency_ms: u128) {
        let Some(emit) = &self.emit else {
            return;
        };
        let mut event = serde_json::json!({
            "type": "auto_review",
            "session_id": req.session_id,
            "decision": verdict.as_str(),
            "model": self.model,
            "ms": latency_ms as u64,
        });
        if let Some(e) = verdict.error() {
            event["error"] = serde_json::json!(e);
        }
        if let Some(r) = verdict.deny_reason() {
            event["reason"] = serde_json::json!(r);
        }
        emit(&event);
    }
}

#[async_trait]
impl Approver for ReviewerApprover<'_> {
    async fn decide(&self, req: ApprovalRequest) -> Decision {
        // Flagged guardrails never reach the reviewer, and a disabled
        // reviewer isn't in the path: both go straight to the human.
        if req.flagged || !self.cfg.resolved_enabled() {
            return self.inner.decide(req).await;
        }
        match self.verdict(&req).await {
            // The one verdict that acts without the human: run once,
            // no prompt, no grant.
            Verdict::Accept => Decision::AllowOnce,
            // Everything else fails closed to the human.
            Verdict::Deny(_) | Verdict::Error(_) => self.inner.decide(req).await,
        }
    }
}

impl ReviewerApprover<'_> {
    /// The review verdict for one request, cached per session+command.
    /// Errors are never cached — a transient failure should retry.
    async fn verdict(&self, req: &ApprovalRequest) -> Verdict {
        let key = (req.session_id.clone(), req.command.clone());
        if let Some(v) = self.cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            return v;
        }
        let start = Instant::now();
        let verdict =
            match tokio::time::timeout(REVIEW_TIMEOUT, self.client.review(&review_context(req)))
                .await
            {
                Ok(Ok(text)) => parse_verdict(&text),
                Ok(Err(e)) => Verdict::Error(e),
                Err(_) => Verdict::Error("timeout".into()),
            };
        let elapsed = start.elapsed().as_millis();
        self.log(req, &verdict, elapsed);
        self.emit(req, &verdict, elapsed);
        if !matches!(verdict, Verdict::Error(_)) {
            if let Ok(mut c) = self.cache.lock() {
                c.insert(key, verdict.clone());
            }
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::ServerApprover;
    use atom_core::session::store::SessionStore;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn request(cmd: &str, flagged: bool) -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".into(),
            command: cmd.into(),
            cwd: PathBuf::from("/ws"),
            workspace_root: PathBuf::from("/ws"),
            rule_id: "unknown-command".into(),
            reason: "r".into(),
            accept_all_preview: None,
            flagged,
        }
    }

    /// Human approver counting prompts and returning a fixed decision.
    struct CountingHuman<'a>(&'a AtomicUsize, Decision);
    #[async_trait]
    impl Approver for CountingHuman<'_> {
        async fn decide(&self, _req: ApprovalRequest) -> Decision {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1
        }
    }

    /// Fake reviewer seam: counts calls, returns a fixed verdict.
    struct FakeReviewer<'a>(&'a AtomicUsize, Result<String, String>);

    #[async_trait]
    impl ReviewClient for FakeReviewer<'_> {
        async fn review(&self, _user_content: &str) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.clone()
        }
    }

    /// Fake reviewer seam that denies negligent commands and accepts
    /// the rest — the `bad` command strings appear verbatim in the
    /// request body's `command:` line.
    struct SelectiveReviewer<'a>(&'a AtomicUsize, &'a [&'a str]);

    #[async_trait]
    impl ReviewClient for SelectiveReviewer<'_> {
        async fn review(&self, user_content: &str) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if self.1.iter().any(|bad| user_content.contains(bad)) {
                Ok("DENY: destructive or negligent".into())
            } else {
                Ok("ACCEPT".into())
            }
        }
    }

    fn cfg(enabled: bool) -> ReviewerConfig {
        ReviewerConfig {
            enabled: Some(enabled),
            ..Default::default()
        }
    }

    fn wrap<'a>(
        human: &'a dyn Approver,
        client: &'a dyn ReviewClient,
        config: ReviewerConfig,
        dir: &'a Path,
    ) -> ReviewerApprover<'a> {
        ReviewerApprover::new(
            human,
            client,
            config,
            "test-model".into(),
            dir.to_path_buf(),
            VerdictCache::default(),
            None,
        )
    }

    #[tokio::test]
    async fn flagged_skips_reviewer_and_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::AllowOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        let d = reviewer.decide(request("rm -rf /tmp/x", true)).await;
        assert_eq!(d, Decision::AllowOnce);
        assert_eq!(llm_calls.load(Ordering::SeqCst), 0, "flagged never reviews");
        assert_eq!(prompts.load(Ordering::SeqCst), 1);
        assert!(!dir.path().join("sandbox-reviewer.log").exists());
    }

    #[tokio::test]
    async fn accept_runs_without_prompting_the_human() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        // Human would refuse; the reviewer's accept must still run it.
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        assert_eq!(
            reviewer.decide(request("cargo build", false)).await,
            Decision::AllowOnce
        );
        assert_eq!(prompts.load(Ordering::SeqCst), 0, "no human prompt");
        assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
        let log =
            std::fs::read_to_string(dir.path().join("sandbox-reviewer.log")).unwrap_or_default();
        let rec: serde_json::Value = serde_json::from_str(log.trim()).expect("one JSONL record");
        assert_eq!(rec["verdict"], "accept");
        assert_eq!(rec["model"], "test-model");
        assert!(rec["cmd_sha256"].is_string());
        assert!(rec["latency_ms"].is_u64());
        assert_eq!(rec["error"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn deny_escalates_to_the_human() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::AllowOnce);
        let fake = FakeReviewer(&llm_calls, Ok("deny".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        // The human's answer is what runs, even though the reviewer
        // denied.
        assert_eq!(
            reviewer.decide(request("cargo build", false)).await,
            Decision::AllowOnce
        );
        assert_eq!(prompts.load(Ordering::SeqCst), 1, "human prompted");
        let log = std::fs::read_to_string(dir.path().join("sandbox-reviewer.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(rec["verdict"], "deny");
    }

    #[tokio::test]
    async fn llm_error_logs_error_and_human_still_decides() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::AllowSession);
        let fake = FakeReviewer(&llm_calls, Err("connection refused".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        let d = reviewer.decide(request("wget http://x", false)).await;
        assert_eq!(d, Decision::AllowSession);
        assert_eq!(prompts.load(Ordering::SeqCst), 1);
        let log = std::fs::read_to_string(dir.path().join("sandbox-reviewer.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(rec["verdict"], "error");
        assert_eq!(rec["error"], "connection refused");
    }

    #[tokio::test]
    async fn unparseable_reply_logs_error() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let fake = FakeReviewer(&llm_calls, Ok("I think this is fine".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        assert_eq!(
            reviewer.decide(request("ls", false)).await,
            Decision::DenyOnce
        );
        let log = std::fs::read_to_string(dir.path().join("sandbox-reviewer.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(rec["verdict"], "error");
        assert!(rec["error"].as_str().unwrap().contains("unparseable"));
    }

    #[tokio::test]
    async fn identical_command_in_same_session_hits_cache() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::AllowOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        for _ in 0..3 {
            assert_eq!(
                reviewer.decide(request("cargo build", false)).await,
                Decision::AllowOnce
            );
        }
        assert_eq!(prompts.load(Ordering::SeqCst), 0, "accepts never prompt");
        assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "LLM billed once");
        // Different command → fresh review. Same command in a different
        // session → fresh review. Errors are never cached.
        assert_eq!(
            reviewer.decide(request("cargo test", false)).await,
            Decision::AllowOnce
        );
        let mut other = request("cargo build", false);
        other.session_id = "s2".into();
        assert_eq!(reviewer.decide(other).await, Decision::AllowOnce);
        assert_eq!(llm_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn error_verdicts_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let llm_calls = AtomicUsize::new(0);
        let prompts = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let fake = FakeReviewer(&llm_calls, Err("boom".into()));
        let reviewer = wrap(&human, &fake, cfg(true), dir.path());
        for _ in 0..2 {
            reviewer.decide(request("curl x", false)).await;
        }
        assert_eq!(llm_calls.load(Ordering::SeqCst), 2, "errors retry");
    }

    #[tokio::test]
    async fn disabled_config_passes_through_silently() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::AllowOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let reviewer = wrap(&human, &fake, cfg(false), dir.path());
        assert_eq!(
            reviewer.decide(request("cargo build", false)).await,
            Decision::AllowOnce
        );
        assert_eq!(llm_calls.load(Ordering::SeqCst), 0);
        assert_eq!(prompts.load(Ordering::SeqCst), 1);
        assert!(!dir.path().join("sandbox-reviewer.log").exists());
    }

    #[tokio::test]
    async fn review_can_never_mint_a_grant() {
        // Through the real gate: an LLM "accept" runs the command once
        // but must never become a session grant the user didn't give.
        let store = atom_sandbox::approvals::ApprovalStore::in_memory();
        let llm_calls = AtomicUsize::new(0);
        struct DenyHuman;
        #[async_trait]
        impl Approver for DenyHuman {
            async fn decide(&self, _req: ApprovalRequest) -> Decision {
                Decision::DenyOnce
            }
        }
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let reviewer = wrap(&DenyHuman, &fake, cfg(true), Path::new("/tmp"));
        let req = request("awk 'BEGIN{print 1}'", false);
        assert_eq!(store.gate(&req, &reviewer).await, Decision::AllowOnce);
        assert_eq!(
            store.classify("awk 'BEGIN{print 1}'"),
            None,
            "no grant minted from the reviewer's accept"
        );
    }

    #[tokio::test]
    async fn timeout_surfaces_as_error_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        struct SlowReviewer;
        #[async_trait]
        impl ReviewClient for SlowReviewer {
            async fn review(&self, _user_content: &str) -> Result<String, String> {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok("ACCEPT".into())
            }
        }
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let reviewer = wrap(&human, &SlowReviewer, cfg(true), dir.path());
        let d = reviewer.decide(request("sleep 30", false)).await;
        assert_eq!(d, Decision::DenyOnce, "human decides despite the stall");
        let log = std::fs::read_to_string(dir.path().join("sandbox-reviewer.log")).unwrap();
        let rec: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(rec["verdict"], "error");
        assert_eq!(rec["error"], "timeout");
    }

    /// Compile-shape check that the decorator wraps the production
    /// approver without borrowing issues (the turn loop's usage).
    #[tokio::test]
    async fn wraps_server_approver() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_in_dir(dir.path().join("sessions")).unwrap();
        let state = Arc::new(crate::state::AppState::new(
            store.into(),
            atom_sandbox::policy::SandboxConfig::default(),
            Arc::new(crate::state::ConnTracker::new()),
        ));
        // with_timeout: nothing answers this prompt, so the cancel
        // token fires after 1s and it resolves to DenyOnce.
        let approver = ServerApprover::with_timeout(state, "s1".into(), Duration::from_secs(1));
        let llm_calls = AtomicUsize::new(0);
        // A reviewer denial escalates into the production approver.
        let fake = FakeReviewer(&llm_calls, Ok("DENY".into()));
        let reviewer = ReviewerApprover::new(
            &approver,
            &fake,
            cfg(true),
            "m".into(),
            dir.path().to_path_buf(),
            VerdictCache::default(),
            None,
        );
        let d = reviewer.decide(request("ls", false)).await;
        assert_eq!(d, Decision::DenyOnce);
        assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reviewer_config_serde_defaults_enabled() {
        let cfg: ReviewerConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.resolved_enabled());
        assert!(cfg.model.is_empty());
        assert_eq!(cfg.reasoning, None);
        let cfg: ReviewerConfig =
            serde_json::from_str(r#"{"enabled":false,"model":"x","reasoning":"high"}"#).unwrap();
        assert!(!cfg.resolved_enabled());
        assert_eq!(cfg.model, "x");
        assert_eq!(cfg.reasoning.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn shared_cache_spans_reviewer_instances() {
        // The turn builds one reviewer per tool call, all sharing this
        // cache, so a repeat command doesn't re-bill the LLM.
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::AllowOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let cache = VerdictCache::default();
        for _ in 0..2 {
            let reviewer = ReviewerApprover::new(
                &human,
                &fake,
                cfg(true),
                "test-model".into(),
                dir.path().to_path_buf(),
                cache.clone(),
                None,
            );
            assert_eq!(
                reviewer.decide(request("cargo build", false)).await,
                Decision::AllowOnce
            );
        }
        assert_eq!(
            prompts.load(Ordering::SeqCst),
            0,
            "cached accepts don't prompt either"
        );
        assert_eq!(
            llm_calls.load(Ordering::SeqCst),
            1,
            "LLM billed once per turn"
        );
    }

    #[tokio::test]
    async fn verdicts_are_emitted_to_the_session_stream() {
        let dir = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            Arc::new(move |ev: &serde_json::Value| events.lock().unwrap().push(ev.clone()))
                as EventSink
        };
        let reviewer = ReviewerApprover::new(
            &human,
            &fake,
            cfg(true),
            "test-model".into(),
            dir.path().to_path_buf(),
            VerdictCache::default(),
            Some(sink),
        );
        reviewer.decide(request("cargo build", false)).await;
        // Flagged commands bypass the reviewer entirely — no event.
        reviewer.decide(request("rm -rf /tmp/x", true)).await;
        let got = events.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "one event per reviewed command: {got:?}");
        assert_eq!(got[0]["type"], "auto_review");
        assert_eq!(got[0]["decision"], "accept");
        assert_eq!(got[0]["model"], "test-model");
        assert!(got[0]["ms"].is_u64());
    }

    #[tokio::test]
    async fn tier2_command_is_reviewed_end_to_end() {
        // The whole path: a Tier-2 command goes through the sandbox gate
        // with the reviewer wrapped around the human approver, runs on
        // the reviewer's accept, and reports the verdict to the sink.
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            Arc::new(move |ev: &serde_json::Value| events.lock().unwrap().push(ev.clone()))
                as EventSink
        };
        let reviewer = ReviewerApprover::new(
            &human,
            &fake,
            cfg(true),
            "session-model".into(),
            data.path().to_path_buf(),
            VerdictCache::default(),
            Some(sink),
        );
        // "awk" is Tier 2: without a reviewer this would prompt.
        let out = atom_sandbox::exec::run_tool_with(
            data.path(),
            "awk 'BEGIN{print 1}'",
            ws.path(),
            ws.path(),
            "e2e-review",
            &atom_sandbox::policy::SandboxConfig::default(),
            &reviewer,
        )
        .await;
        assert!(out.approved, "reviewer accept runs the command");
        assert_eq!(prompts.load(Ordering::SeqCst), 0, "no human prompt");
        assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
        let got = events.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "verdict reported: {got:?}");
        assert_eq!(got[0]["decision"], "accept");
    }

    #[tokio::test]
    async fn flagged_command_is_never_reviewed_end_to_end() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let prompts = AtomicUsize::new(0);
        let llm_calls = AtomicUsize::new(0);
        let human = CountingHuman(&prompts, Decision::DenyOnce);
        let fake = FakeReviewer(&llm_calls, Ok("ACCEPT".into()));
        let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            Arc::new(move |ev: &serde_json::Value| events.lock().unwrap().push(ev.clone()))
                as EventSink
        };
        let reviewer = ReviewerApprover::new(
            &human,
            &fake,
            cfg(true),
            "session-model".into(),
            data.path().to_path_buf(),
            VerdictCache::default(),
            Some(sink),
        );
        let out = atom_sandbox::exec::run_tool_with(
            data.path(),
            "rm -rf /tmp/atom-e2e-nonexistent",
            ws.path(),
            ws.path(),
            "e2e-flagged",
            &atom_sandbox::policy::SandboxConfig::default(),
            &reviewer,
        )
        .await;
        assert!(!out.approved, "flagged command waits for the human");
        assert_eq!(prompts.load(Ordering::SeqCst), 1, "human asked");
        assert_eq!(llm_calls.load(Ordering::SeqCst), 0, "reviewer skipped");
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn parse_verdict_cases() {
        assert_eq!(parse_verdict("ACCEPT"), Verdict::Accept);
        assert_eq!(parse_verdict(" accept "), Verdict::Accept);
        assert_eq!(parse_verdict("Deny"), Verdict::Deny(String::new()));
        assert_eq!(
            parse_verdict("DENY: needs network to api.example.com"),
            Verdict::Deny("needs network to api.example.com".into())
        );
        assert_eq!(
            parse_verdict("deny - not reversible"),
            Verdict::Deny("not reversible".into())
        );
        assert!(matches!(parse_verdict(""), Verdict::Error(_)));
        assert!(matches!(parse_verdict("ACCEPT DENY"), Verdict::Error(_)));
        assert!(matches!(parse_verdict("maybe"), Verdict::Error(_)));
    }

    /// The headline property: auto-review absorbs the routine prompts
    /// a bare gate would ask, while negligent commands and guardrail
    /// hits still reach the human. Ten commands, three fates:
    ///
    /// - 8 routine Tier-2 commands the reviewer accepts — no prompt.
    /// - `rm -rf node_modules` — reviewer denies, human prompted.
    /// - `killall Dock` — guardrail-flagged, skips the reviewer
    ///   entirely, human prompted.
    #[tokio::test]
    async fn auto_review_absorbs_routine_and_escalates_negligent_prompts() {
        let dir = tempfile::tempdir().unwrap();
        // (command, flagged) — flagged stands in for the guardrail
        // floor the static table would apply to `killall`.
        let commands = [
            ("cargo build --release", false),
            ("ls -la src/deep/nested/dir", false),
            ("grep -rn TODO .", false),
            ("git status", false),
            ("make check", false),
            ("python -m pytest tests/", false),
            ("docker compose logs api", false),
            ("jq . package.json", false),
            ("rm -rf node_modules", false),
            ("killall Dock", true),
        ];
        let negligent = ["rm -rf node_modules"];

        let without = {
            let prompts = AtomicUsize::new(0);
            let human = CountingHuman(&prompts, Decision::AllowOnce);
            for (cmd, flagged) in commands {
                human.decide(request(cmd, flagged)).await;
            }
            prompts.load(Ordering::SeqCst)
        };

        let (with, llm_calls) = {
            let prompts = AtomicUsize::new(0);
            let llm_calls = AtomicUsize::new(0);
            let human = CountingHuman(&prompts, Decision::AllowOnce);
            let fake = SelectiveReviewer(&llm_calls, &negligent);
            let reviewer = wrap(&human, &fake, cfg(true), dir.path());
            for (cmd, flagged) in commands {
                reviewer.decide(request(cmd, flagged)).await;
            }
            (
                prompts.load(Ordering::SeqCst),
                llm_calls.load(Ordering::SeqCst),
            )
        };

        assert_eq!(
            without,
            commands.len(),
            "no reviewer: every command prompts"
        );
        assert_eq!(with, 2, "only the negligent and flagged commands prompt");
        assert_eq!(
            llm_calls,
            commands.len() - 1,
            "the reviewer sees every command but the flagged one"
        );
    }
}
