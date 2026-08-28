//! Dispatch bridge + sandbox approval bridge, ported from dispatch.go
//! and the server-side approval flow. The dispatch tool routes here via
//! atom_tools' SubagentHandle; the sandbox pipeline consults the
//! ServerApprover, which surfaces an `approval_request` event on the
//! active turn and session streams and awaits POST /approval/:session (60s timeout ->
//! Deny).

use crate::instructions::load_instructions_from;
use crate::state::AppState;
use async_trait::async_trait;
use atom_core::session::store::{first_line_trunc, DelegateStatus, Session, SessionInfo};
use atom_sandbox::approvals::{ApprovalRequest, Approver, Decision};
use atom_tools::{DispatchPlan, SubagentHandle};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default timeout for test approvers (production approvals block indefinitely).
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// dispatch result: how long `wait:true` blocks for a subagent's turn to
/// finish before returning the current state.
const RESULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// dispatch result: poll interval while waiting.
const RESULT_WAIT_POLL: Duration = Duration::from_millis(200);
/// dispatch result: longest transcript tail returned to the main agent.
const RESULT_TAIL_LIMIT: usize = 8000;

/// formatChildResult renders a subagent's status and latest answer for the
/// dispatch `get_result` tool. The tail is the final non-empty assistant
/// message, so an aggregation parent can read what the child produced.
fn child_result(child: &Session, include_result: bool) -> serde_json::Value {
    let mut tail = String::new();
    for m in child.messages.iter().rev() {
        if m.role == "assistant" && !m.content.trim().is_empty() {
            tail = m.content.clone();
            break;
        }
        // User-initiated stop (Esc in the TUI): bookkeeping, not an error.
        if m.role == "stopped" && !m.content.trim().is_empty() {
            tail = m.content.clone();
            break;
        }
        if m.role == "error" && !m.content.trim().is_empty() {
            tail = format!("error: {}", m.content);
            break;
        }
    }
    if tail.is_empty() {
        tail = "(no assistant reply yet)".to_string();
    }
    if tail.len() > RESULT_TAIL_LIMIT {
        let mut cut = RESULT_TAIL_LIMIT.saturating_sub(1);
        while cut > 0 && !tail.is_char_boundary(cut) {
            cut -= 1;
        }
        tail = format!("{}…", &tail[..cut]);
    }
    let mut value = json!({
        "index": child.batch_index,
        "id": child.id,
        "model": child.model,
        "thinking": child.thinking,
        "status": child.status.as_str(),
        "message_count": child.messages.len(),
    });
    if include_result {
        value["result"] = json!(tail);
    }
    value
}

fn status_snapshot(batch_id: &str, children: &[Session], include_results: bool) -> String {
    let mut counts = serde_json::Map::new();
    for name in ["queued", "working", "sandbox", "error", "done", "cancelled"] {
        counts.insert(
            name.into(),
            json!(children
                .iter()
                .filter(|child| child.status.as_str() == name)
                .count()),
        );
    }
    json!({
        "batch_id": batch_id,
        "counts": counts,
        "delegates": children.iter().map(|child| child_result(child, include_results)).collect::<Vec<_>>(),
    })
    .to_string()
}

fn status_snapshot_info(batch_id: &str, children: &[SessionInfo]) -> String {
    let mut counts = serde_json::Map::new();
    for name in ["queued", "working", "sandbox", "error", "done", "cancelled"] {
        counts.insert(
            name.into(),
            json!(children
                .iter()
                .filter(|child| child.status.as_str() == name)
                .count()),
        );
    }
    json!({
        "batch_id": batch_id,
        "counts": counts,
        "delegates": children.iter().map(|child| json!({
            "index": child.batch_index,
            "id": child.id,
            "model": child.model,
            "thinking": child.thinking,
            "status": child.status.as_str(),
            "message_count": child.message_count,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Approval bridge.
// ---------------------------------------------------------------------------

/// The Approver implementation the server hands to every bash tool call:
/// broadcasts `{"type":"approval_request","id","session_id","command",
/// "cwd","rule_id","reason"}` on the session channel (and the parent's,
/// when the session is a dispatched subagent) and awaits the user's
/// decision. AllowGlobal recording happens in the sandbox's shared
/// ApprovalStore when this decision flows back through its gate.
pub struct ServerApprover {
    state: Arc<AppState>,
    session_id: String,
    out: crate::turn::EventOut,
    /// Cancel token for the current turn — allows Esc/pause to interrupt
    /// an indefinitely-blocking approval prompt.
    cancel: Option<crate::cancel::CancelToken>,
}

/// Builds the `approval_request` event for one pending prompt. When the
/// session is a dispatched subagent, the event carries the child's
/// identity (`from_subagent`, `child_title`) so the parent view can
/// surface the prompt and answer it without navigating into the child.
pub fn approval_request_event(
    state: &AppState,
    session_id: &str,
    id: &str,
    req: &ApprovalRequest,
) -> serde_json::Value {
    let mut ev = json!({
        "type": "approval_request",
        "id": id,
        "session_id": session_id,
        "command": req.command,
        "cwd": req.cwd.display().to_string(),
        "rule_id": req.rule_id,
        "reason": req.reason,
    });
    if let Some(child) = state.store.get_info(session_id) {
        if !child.parent_id.is_empty() {
            ev["from_subagent"] = json!(true);
            let title = if !child.title.is_empty() {
                child.title.clone()
            } else if !child.model.is_empty() {
                child.model.clone()
            } else {
                let short: String = child.id.chars().take(8).collect();
                short
            };
            ev["child_title"] = json!(title);
        }
    }
    ev
}

impl ServerApprover {
    pub fn new(state: Arc<AppState>, session_id: String) -> Self {
        ServerApprover {
            state,
            session_id,
            out: crate::turn::EventOut::Discard,
            cancel: None,
        }
    }

    pub fn for_turn(
        state: Arc<AppState>,
        session_id: String,
        out: crate::turn::EventOut,
        cancel: Option<crate::cancel::CancelToken>,
    ) -> Self {
        ServerApprover {
            state,
            session_id,
            out,
            cancel,
        }
    }

    /// Test/short-timeout variant (uses a fixed timeout instead of blocking
    /// indefinitely).
    pub fn with_timeout(state: Arc<AppState>, session_id: String, timeout: Duration) -> Self {
        // For tests: use a cancellation token that fires after `timeout`.
        let cancel = crate::cancel::CancelToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            c2.cancel();
        });
        ServerApprover {
            state,
            session_id,
            out: crate::turn::EventOut::Discard,
            cancel: Some(cancel),
        }
    }
}

#[async_trait]
impl Approver for ServerApprover {
    async fn decide(&self, req: ApprovalRequest) -> Decision {
        let id = atom_core::session::store::new_session_id();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.state
            .approvals
            .register(&self.session_id, &id, req.clone(), tx);
        let child = self.state.store.get_info(&self.session_id);
        let parent_id = child
            .as_ref()
            .filter(|session| !session.parent_id.is_empty())
            .map(|session| session.parent_id.clone());
        if let Some(parent_id) = &parent_id {
            let session_id = self.session_id.clone();
            self.state
                .store_call(move |store| {
                    store.update_delegate_status(&session_id, DelegateStatus::Sandbox)
                })
                .await;
            self.state
                .subs
                .broadcast(parent_id, &json!({"type": "children"}));
        }
        let ev = approval_request_event(&self.state, &self.session_id, &id, &req);
        crate::turn::emit(&self.state, &self.out, &self.session_id, &ev).await;
        // The parent hears about the request too, so the user can answer
        // it without navigating into the subagent.
        if let Some(parent_id) = &parent_id {
            self.state.subs.broadcast(parent_id, &ev);
        }
        let decision = match &self.cancel {
            Some(cancel) => {
                tokio::select! {
                    res = rx => res.unwrap_or(Decision::Deny),
                    _ = cancel.cancelled() => Decision::Deny,
                }
            }
            None => {
                // No cancel token: block indefinitely until user responds
                // or the oneshot sender is dropped (session cleanup).
                rx.await.unwrap_or(Decision::Deny)
            }
        };
        self.state.approvals.remove(&self.session_id, &id);
        if let Some(child) = self.state.store.get_info(&self.session_id) {
            if !child.parent_id.is_empty() && !child.cancelled {
                let session_id = self.session_id.clone();
                self.state
                    .store_call(move |store| {
                        store.update_delegate_status(&session_id, DelegateStatus::Working)
                    })
                    .await;
                self.state
                    .subs
                    .broadcast(&child.parent_id, &json!({"type": "children"}));
            }
        }
        decision
    }
}

// ---------------------------------------------------------------------------
// Dispatch bridge.
// ---------------------------------------------------------------------------

/// The server's SubagentHandle: creates child sessions, posts follow-ups,
/// cancels active turns. `parent_id` is the session whose turn is running
/// the dispatch tool; key/base_url/reasoning_field carry the caller's
/// provider plumbing to the detached child turn.
pub struct DispatchBridge {
    state: Arc<AppState>,
    parent_id: String,
    parent_cancel: crate::cancel::CancelToken,
    key: String,
    base_url: String,
    reasoning_field: String,
}

impl DispatchBridge {
    pub fn new(
        state: Arc<AppState>,
        parent_id: String,
        parent_cancel: crate::cancel::CancelToken,
        key: String,
        base_url: String,
        reasoning_field: String,
    ) -> Self {
        DispatchBridge {
            state,
            parent_id,
            parent_cancel,
            key,
            base_url,
            reasoning_field,
        }
    }

    /// ownedDispatchChild validates shape, existence, and ownership.
    fn owned_child_info(&self, child_id: &str) -> Result<SessionInfo, String> {
        if !atom_tools::is_dispatch_session_id(child_id) {
            return Err("error: session_id must be a 16-character hex session id".into());
        }
        let Some(child) = self.state.store.get_info(child_id) else {
            return Err("error: subagent not found".into());
        };
        if !self.state.store.is_descendant_of(child_id, &self.parent_id) {
            return Err("error: not your subagent".into());
        }
        Ok(child)
    }

    async fn owned_child(&self, child_id: &str) -> Result<Session, String> {
        self.owned_child_info(child_id)?;
        let child_id = child_id.to_string();
        self.state
            .store_call(move |store| store.get(&child_id))
            .await
            .ok_or_else(|| "error: subagent not found".into())
    }

    fn current_provider(&self) -> atom_core::providers::Provider {
        let name = atom_core::providers::provider_name_for_url(&self.base_url);
        let id = if matches!(name.as_str(), "custom" | "ollama-local") {
            String::new()
        } else {
            atom_core::providers::modelsdev::models_dev_provider_id(&name)
        };
        atom_core::providers::Provider {
            name,
            id,
            base_url: self.base_url.clone(),
            key: self.key.clone(),
            reasoning_field: self.reasoning_field.clone(),
        }
    }

    async fn resolve_provider(
        &self,
        requested: &str,
    ) -> Result<atom_core::providers::Provider, String> {
        let current = self.current_provider();
        if requested.is_empty() || requested == current.name || requested == current.id {
            return Ok(current);
        }
        atom_core::providers::providers::build_providers()
            .await
            .into_iter()
            .find(|provider| provider.name == requested || provider.id == requested)
            .ok_or_else(|| format!("error: provider \"{requested}\" is not configured"))
    }

    fn catalog_provider(provider: &atom_core::providers::Provider) -> Option<String> {
        if matches!(provider.name.as_str(), "custom" | "ollama-local") {
            return None;
        }
        let catalog_id = atom_core::providers::modelsdev::provider_catalog_id(provider);
        atom_core::providers::modelsdev::models_dev_provider_ids()
            .iter()
            .any(|id| id == &catalog_id)
            .then_some(provider.name.clone())
    }

    async fn validate_model(provider: &atom_core::providers::Provider, model: &str) -> String {
        let Some(catalog_provider) = Self::catalog_provider(provider) else {
            return String::new();
        };
        if atom_core::providers::modelsdev::find_catalog_model(&catalog_provider, model).is_some() {
            return String::new();
        }
        if atom_core::providers::providers::fetch_models(provider)
            .await
            .is_ok_and(|models| models.iter().any(|candidate| candidate == model))
        {
            return String::new();
        }
        let suggestions = atom_core::providers::modelsdev::suggest_catalog_model_ids(model, 3);
        if suggestions.is_empty() {
            format!("error: unknown model \"{model}\"")
        } else {
            format!(
                "error: unknown model \"{model}\" (did you mean {}?)",
                suggestions.join(", ")
            )
        }
    }
}

#[async_trait]
impl SubagentHandle for DispatchBridge {
    /// executeDispatch spawn path: inherit the caller's model, validate,
    /// create the child with fresh instructions, append the prompt, and
    /// kick off a detached turn.
    async fn spawn(&self, plan: DispatchPlan) -> String {
        // The daemon does not retain the multi-megabyte catalog at idle.
        // Dispatch is the first server-only operation that needs it.
        atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
        let parent_id = self.parent_id.clone();
        let Some(parent) = self
            .state
            .store_call(move |store| store.get(&parent_id))
            .await
        else {
            return "error: dispatch requires an active session".into();
        };

        // Go inherits the caller's model BEFORE validating it.
        let model = if plan.model.is_empty() {
            parent.model.trim().to_string()
        } else {
            plan.model.clone()
        };
        if model.is_empty() {
            return "error: model is required (no caller model to inherit)".into();
        }
        let provider = match self.resolve_provider(&plan.provider).await {
            Ok(provider) => provider,
            Err(err) => return err,
        };
        let err = Self::validate_model(&provider, &model).await;
        if !err.is_empty() {
            return err;
        }
        let catalog_provider = Self::catalog_provider(&provider).unwrap_or_default();
        if !atom_tools::dispatch::valid_thinking_level(&catalog_provider, &model, &plan.thinking) {
            let levels = atom_tools::dispatch::reasoning_levels_for(&catalog_provider, &model);
            if !levels.is_empty() {
                return format!("error: thinking must be one of {}", levels.join(", "));
            }
            return "error: thinking must be a reasoning_effort value for this model".into();
        }
        if !parent.parent_id.is_empty() {
            return "error: subagents cannot dispatch nested subagents".into();
        }
        let cwd = parent.cwd.clone();
        if !std::path::Path::new(&cwd).is_absolute() {
            return "error: parent session has no absolute cwd".into();
        }
        let title = first_line_trunc(&plan.prompt, 60);
        let store_parent_id = parent.id.clone();
        let store_model = model.clone();
        let store_cwd = cwd.clone();
        let store_thinking = plan.thinking.clone();
        let store_title = title.clone();
        let store_provider = provider.name.clone();
        let store_batch_id = plan.batch_id.clone();
        let store_batch_index = plan.batch_index;
        let store_prompt = plan.prompt.clone();
        let child = self
            .state
            .store_call(move |store| {
                let instr = load_instructions_from(&store_cwd);
                let mut child = store.create_child(
                    &store_parent_id,
                    &store_model,
                    &store_cwd,
                    &store_thinking,
                    &store_title,
                    instr,
                );
                store.update_provider(&child.id, &store_provider);
                store.update_delegate_batch(&child.id, &store_batch_id, store_batch_index);
                child.provider = store_provider;
                child.batch_id = store_batch_id;
                child.batch_index = store_batch_index as i64;
                let messages = vec![atom_core::types::Message {
                    role: "user".into(),
                    content: store_prompt,
                    ..Default::default()
                }];
                store.update(&child.id, messages, &child.title);
                child
            })
            .await;
        self.state.turns.prepare_session_turn(&child.id);
        kickoff_dispatch_turn(
            &self.state,
            &child.id,
            &plan.thinking,
            &provider.key,
            &provider.base_url,
            &provider.reasoning_field,
        );
        json!({
            "index": child.batch_index,
            "id": child.id,
            "provider": provider.name,
            "model": child.model,
            "thinking": child.thinking,
            "status": "queued",
        })
        .to_string()
    }

    /// continueDispatch posts a follow-up prompt to an owned, idle child.
    async fn cont(&self, plan: DispatchPlan) -> String {
        atom_core::providers::modelsdev::ensure_models_dev_catalog().await;
        let child = match self.owned_child(&plan.session_id).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        if plan.prompt.is_empty() {
            return "error: prompt is required".into();
        }
        let mut thinking = plan.thinking.clone();
        if thinking.is_empty() {
            thinking = child.thinking.trim().to_string();
        }
        let provider = match self.resolve_provider(&child.provider).await {
            Ok(provider) => provider,
            Err(err) => return err,
        };
        let catalog_provider = Self::catalog_provider(&provider).unwrap_or_default();
        if !thinking.is_empty()
            && !atom_tools::dispatch::valid_thinking_level(
                &catalog_provider,
                &child.model,
                &thinking,
            )
        {
            let levels =
                atom_tools::dispatch::reasoning_levels_for(&catalog_provider, &child.model);
            if !levels.is_empty() {
                return format!("error: thinking must be one of {}", levels.join(", "));
            }
            return "error: thinking must be a reasoning_effort value for this model".into();
        }
        if child.status.is_active() || self.state.turns.session_has_active_turn(&child.id) {
            return "error: subagent is still active; cancel it before sending a follow-up".into();
        }
        self.state.turns.clear_pending_pauses(&child.id);
        let mut messages = child.messages.clone();
        messages.push(atom_core::types::Message {
            role: "user".into(),
            content: plan.prompt.clone(),
            ..Default::default()
        });
        let child_id = child.id.clone();
        let was_cancelled = child.cancelled;
        let update_thinking = !thinking.is_empty() && thinking != child.thinking;
        let stored_thinking = thinking.clone();
        self.state
            .store_call(move |store| {
                // A follow-up explicitly revives a previously killed subagent.
                if was_cancelled {
                    store.set_cancelled(&child_id, false);
                }
                if update_thinking {
                    store.update_thinking(&child_id, &stored_thinking);
                }
                store.update(&child_id, messages, "");
                store.update_delegate_status(&child_id, DelegateStatus::Queued);
            })
            .await;
        // Show the follow-up to every client viewing the child (and the
        // parent's panel) immediately, not after the child's turn ends.
        // The child's detached turn runs with skip_append, so this is the
        // only place the user_message event can be emitted for it.
        self.state.subs.broadcast(
            &child.id,
            &json!({"type": "user_message", "text": plan.prompt.clone()}),
        );
        self.state
            .subs
            .broadcast(&self.parent_id, &json!({"type": "children"}));
        self.state.turns.prepare_session_turn(&child.id);
        kickoff_dispatch_turn(
            &self.state,
            &child.id,
            &thinking,
            &provider.key,
            &provider.base_url,
            &provider.reasoning_field,
        );
        format!(
            "ok: continued\nid: {}\nmessage_count: {}",
            child.id,
            child.messages.len() + 1
        )
    }

    async fn result(&self, plan: DispatchPlan) -> String {
        let load_target_infos = || -> Result<Vec<SessionInfo>, String> {
            if !plan.session_id.is_empty() {
                return self
                    .owned_child_info(&plan.session_id)
                    .map(|child| vec![child]);
            }
            let mut children: Vec<SessionInfo> = self
                .state
                .store
                .children_info(&self.parent_id)
                .into_iter()
                .filter(|child| plan.batch_id.is_empty() || child.batch_id == plan.batch_id)
                .filter(|child| plan.ids.is_empty() || plan.ids.iter().any(|id| id == &child.id))
                .collect();
            children.sort_by_key(|child| child.batch_index);
            if !plan.ids.is_empty() && children.len() != plan.ids.len() {
                return Err("error: one or more subagents were not found or are not yours".into());
            }
            Ok(children)
        };
        let wait_mode = plan.wait_mode.as_str();
        if matches!(wait_mode, "any" | "all") {
            let deadline = Instant::now() + RESULT_WAIT_TIMEOUT;
            loop {
                let targets = match load_target_infos() {
                    Ok(children) => children,
                    Err(error) => return error,
                };
                let terminal = targets
                    .iter()
                    .filter(|child| !child.status.is_active())
                    .count();
                if (wait_mode == "any" && terminal > 0)
                    || (wait_mode == "all" && terminal == targets.len())
                {
                    break;
                }
                if self.parent_cancel.is_cancelled() || Instant::now() > deadline {
                    break;
                }
                tokio::select! {
                    _ = self.parent_cancel.cancelled() => break,
                    _ = tokio::time::sleep(RESULT_WAIT_POLL) => {}
                }
            }
        }
        let infos = match load_target_infos() {
            Ok(children) => children,
            Err(error) => return error,
        };
        if !plan.include_results {
            return status_snapshot_info(&plan.batch_id, &infos);
        }
        let ids: Vec<String> = infos.iter().map(|info| info.id.clone()).collect();
        let children: Vec<Session> = self
            .state
            .store_call(move |store| ids.iter().filter_map(|id| store.get(id)).collect())
            .await;
        status_snapshot(&plan.batch_id, &children, true)
    }

    /// cancelDispatch pauses the owned child so its turn stops, then
    /// records it as killed so it drops out of the parent's children
    /// listing. The session record stays on disk (and revivable via a
    /// follow-up), but the TUI only shows subagents that were never
    /// explicitly killed.
    async fn cancel(&self, sid: &str) -> String {
        if let Err(e) = self.owned_child_info(sid) {
            return e;
        }
        pause_dispatch_session(&self.state, sid);
        let sid_owned = sid.to_string();
        self.state
            .store_call(move |store| {
                store.set_cancelled(&sid_owned, true);
                store.update_delegate_status(&sid_owned, DelegateStatus::Cancelled);
            })
            .await;
        self.state
            .subs
            .broadcast(&self.parent_id, &json!({"type": "children"}));
        format!("ok: cancelled\nid: {sid}")
    }
}

/// pauseDispatchSession cancels a live turn outright, or records the
/// pending "dispatch-<id>" pause so a just-kickoff'd child dies at once.
fn pause_dispatch_session(state: &AppState, id: &str) {
    if state.turns.session_has_active_turn(id) {
        state.turns.pause_session(id, "");
        return;
    }
    state.turns.pause_session(id, &format!("dispatch-{id}"));
}

/// kickoffDispatchTurn runs the child turn detached from any HTTP
/// request: other viewers may subscribe normally, but the detached task
/// itself does not create an undrained subscriber queue. It writes nothing
/// to a response and never cancels via parent context (context.Background()).
pub fn kickoff_dispatch_turn(
    state: &Arc<AppState>,
    id: &str,
    thinking: &str,
    key: &str,
    base_url: &str,
    reasoning_field: &str,
) {
    let state = state.clone();
    let id = id.to_string();
    // Keep the parent's provider credentials for a potential auto-continue
    // turn after the child finishes (dispatch inherits the caller's provider).
    let parent_key = key.to_string();
    let parent_base_url = base_url.to_string();
    let parent_reasoning_field = reasoning_field.to_string();
    let opts = crate::turn::TurnOpts {
        message: String::new(),
        thinking: thinking.to_string(),
        key: key.to_string(),
        base_url: base_url.to_string(),
        reasoning_field: reasoning_field.to_string(),
        turn_id: format!("dispatch-{id}"),
        images: Vec::new(),
        compact: false,
        compact_instructions: String::new(),
        skip_append: true,
    };
    tokio::spawn(async move {
        let out = crate::turn::EventOut::Discard;
        let load_id = id.clone();
        let mut sess = match state.store_call(move |store| store.get(&load_id)).await {
            Some(s) => s,
            None => return,
        };
        crate::turn::run_session_turn_guarded(
            &state,
            &mut sess,
            &id,
            opts,
            out,
            crate::cancel::CancelToken::new(),
        )
        .await;
        // The child turn has finished: notify the parent session so the
        // main agent (and the TUI) learn about completion without polling.
        let parent_id = sess.parent_id.clone();
        if !parent_id.is_empty() {
            let child_status = state
                .store
                .get_info(&id)
                .map(|child| child.status)
                .unwrap_or(DelegateStatus::Error);
            state.subs.broadcast(
                &parent_id,
                &json!({"type": "dispatch_result", "child": id, "status": child_status.as_str()}),
            );
            // Auto-continue: when the child reached a terminal state and the
            // parent session is idle, start a follow-up turn so the parent
            // agent can inspect results and report to the user.
            if !child_status.is_active() {
                maybe_auto_continue_parent(
                    &state,
                    &parent_id,
                    &parent_key,
                    &parent_base_url,
                    &parent_reasoning_field,
                )
                .await;
            }
        }
    });
}

/// Small delay before auto-continuing the parent to batch multiple
/// near-simultaneous subagent completions into one parent turn.
const AUTO_CONTINUE_DEBOUNCE: Duration = Duration::from_millis(600);

/// Attempt to auto-continue the parent session when one or more subagents
/// reach a terminal state. Uses `try_prepare_session_turn` for atomic
/// reservation so concurrent completions don't race: only the first one
/// that wins the reservation triggers the turn. Only fires when ALL alive
/// (non-cancelled) children have reached a terminal state so the parent
/// gets one clean turn with complete results rather than being woken
/// repeatedly for partial completions.
async fn maybe_auto_continue_parent(
    state: &Arc<AppState>,
    parent_id: &str,
    key: &str,
    base_url: &str,
    reasoning_field: &str,
) {
    // Short debounce: if multiple subagents finish around the same time,
    // we want one parent turn that sees them all rather than several turns
    // each seeing only one.
    tokio::time::sleep(AUTO_CONTINUE_DEBOUNCE).await;

    // Gather children status to decide whether to proceed.
    let children: Vec<SessionInfo> = state
        .store
        .children_info(parent_id)
        .into_iter()
        .filter(|child| !child.cancelled)
        .collect();

    if children.is_empty() {
        return;
    }

    // Only auto-continue when ALL alive children are terminal (done/error).
    let all_terminal = children.iter().all(|c| !c.status.is_active());
    if !all_terminal {
        return;
    }

    // Atomically reserve the parent — if it's already running (user sent
    // a message, or another auto-continue won the race) we bail out.
    if !state.turns.try_prepare_session_turn(parent_id) {
        return;
    }

    let done_count = children
        .iter()
        .filter(|c| c.status == DelegateStatus::Done)
        .count();
    let error_count = children
        .iter()
        .filter(|c| c.status == DelegateStatus::Error)
        .count();
    let total = children.len();

    let status_summary = if error_count > 0 {
        format!("{done_count} done, {error_count} errored (out of {total})")
    } else {
        format!("all {total} done")
    };

    let notification = format!(
        "[system: your subagents have finished ({status_summary}). \
         Use dispatch action=inspect to collect their results and report back to the user.]"
    );

    let load_id = parent_id.to_string();
    let Some(mut parent_sess) = state.store_call(move |store| store.get(&load_id)).await else {
        state.turns.release_prepared(parent_id);
        return;
    };

    let parent_thinking = parent_sess.thinking.clone();
    let parent_id_owned = parent_id.to_string();
    let opts = crate::turn::TurnOpts {
        message: notification,
        thinking: parent_thinking,
        key: key.to_string(),
        base_url: base_url.to_string(),
        reasoning_field: reasoning_field.to_string(),
        turn_id: format!("auto-continue-{parent_id_owned}"),
        images: Vec::new(),
        compact: false,
        compact_instructions: String::new(),
        skip_append: false,
    };
    crate::turn::run_session_turn_guarded(
        state,
        &mut parent_sess,
        &parent_id_owned,
        opts,
        crate::turn::EventOut::Discard,
        crate::cancel::CancelToken::new(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use atom_core::session::store::SessionStore;
    use std::sync::Arc;

    fn test_state(dir: &std::path::Path) -> Arc<AppState> {
        let store = Arc::new(SessionStore::open_in_dir(dir).unwrap());
        Arc::new(AppState::new(
            store,
            atom_sandbox::policy::SandboxConfig::default(),
            Arc::new(crate::state::ConnTracker::default()),
        ))
    }

    #[tokio::test]
    async fn dispatch_result_returns_tail_and_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let parent = state.store.create("m", "/tmp", vec![]);
        let child = state
            .store
            .create_child(&parent.id, "m", "/tmp", "low", "child", vec![]);
        state.store.update(
            &child.id,
            vec![atom_core::types::Message {
                role: "assistant".into(),
                content: "The answer is 42".into(),
                ..Default::default()
            }],
            "",
        );
        state
            .store
            .update_delegate_status(&child.id, DelegateStatus::Done);

        let bridge = DispatchBridge::new(
            state.clone(),
            parent.id.clone(),
            crate::cancel::CancelToken::new(),
            String::new(),
            String::new(),
            String::new(),
        );
        let plan = DispatchPlan {
            session_id: child.id.clone(),
            include_results: true,
            ..Default::default()
        };
        let out = bridge.result(plan).await;
        assert!(out.contains("\"status\":\"done\""), "{}", out);
        assert!(out.contains("The answer is 42"), "{}", out);

        // A non-owner must not read a child's result.
        let other_parent = state.store.create("m", "/tmp", vec![]);
        let other_child =
            state
                .store
                .create_child(&other_parent.id, "m", "/tmp", "low", "other", vec![]);
        let foreign = DispatchBridge::new(
            state.clone(),
            parent.id.clone(),
            crate::cancel::CancelToken::new(),
            String::new(),
            String::new(),
            String::new(),
        );
        let out = foreign
            .result(DispatchPlan {
                session_id: other_child.id.clone(),
                ..Default::default()
            })
            .await;
        assert_eq!(out, "error: not your subagent");
    }

    #[test]
    fn format_result_surfaces_persisted_provider_error() {
        let mut child = atom_core::session::store::Session {
            id: "0123456789abcdef".into(),
            ..Default::default()
        };
        child.messages.push(atom_core::types::Message {
            role: "error".into(),
            content: "provider returned 400".into(),
            ..Default::default()
        });

        child.status = DelegateStatus::Error;
        let result = child_result(&child, true);
        assert_eq!(result["result"], "error: provider returned 400");
    }

    #[test]
    fn format_result_reports_user_stop_without_error_prefix() {
        let mut child = atom_core::session::store::Session {
            id: "0123456789abcdef".into(),
            ..Default::default()
        };
        child.messages.push(atom_core::types::Message {
            role: "assistant".into(),
            content: "partial work".into(),
            ..Default::default()
        });
        child.messages.push(atom_core::types::Message {
            role: "stopped".into(),
            content: "stopped by the user".into(),
            ..Default::default()
        });

        child.status = DelegateStatus::Stopped;
        let result = child_result(&child, true);
        assert_eq!(result["result"], "stopped by the user");
        assert_eq!(result["status"], "stopped");
    }

    #[tokio::test]
    async fn inspect_returns_all_children_with_status_counts() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let parent = state.store.create("m", "/tmp", vec![]);
        let done = state
            .store
            .create_child(&parent.id, "m1", "/tmp", "low", "done", vec![]);
        let working = state
            .store
            .create_child(&parent.id, "m2", "/tmp", "high", "working", vec![]);
        state.store.update_delegate_batch(&done.id, "batch", 1);
        state.store.update_delegate_batch(&working.id, "batch", 2);
        state
            .store
            .update_delegate_status(&done.id, DelegateStatus::Done);
        state
            .store
            .update_delegate_status(&working.id, DelegateStatus::Working);

        let bridge = DispatchBridge::new(
            state,
            parent.id,
            crate::cancel::CancelToken::new(),
            String::new(),
            String::new(),
            String::new(),
        );
        let out = bridge
            .result(DispatchPlan {
                batch_id: "batch".into(),
                include_results: false,
                ..Default::default()
            })
            .await;
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["counts"]["done"], 1);
        assert_eq!(value["counts"]["working"], 1);
        assert_eq!(value["delegates"].as_array().unwrap().len(), 2);
        assert_eq!(value["delegates"][0]["index"], 1);
    }

    #[test]
    fn approval_event_tags_dispatch_children_with_identity() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let parent = state.store.create("m", "/tmp", vec![]);
        let child =
            state
                .store
                .create_child(&parent.id, "m", "/tmp", "low", "verify build", vec![]);
        let req = ApprovalRequest {
            session_id: child.id.clone(),
            command: "git push".into(),
            cwd: "/repo".into(),
            rule_id: "git-push".into(),
            reason: "push to remote".into(),
        };

        // A plain session's event carries no subagent identity.
        let plain = approval_request_event(&state, &parent.id, "a1", &req);
        assert_eq!(plain["session_id"], parent.id);
        assert_ne!(plain.get("from_subagent"), Some(&json!(true)));

        // A dispatch child's event names it so the parent view can
        // surface the prompt.
        let ev = approval_request_event(&state, &child.id, "a2", &req);
        assert_eq!(ev["session_id"], child.id);
        assert_eq!(ev["from_subagent"], json!(true));
        assert_eq!(ev["child_title"], "verify build");
        assert_eq!(ev["command"], "git push");
        assert_eq!(ev["id"], "a2");
    }

    #[tokio::test]
    async fn pending_approvals_replay_request_details() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let parent = state.store.create("m", "/tmp", vec![]);
        let child = state
            .store
            .create_child(&parent.id, "m", "/tmp", "low", "child", vec![]);
        let req = ApprovalRequest {
            session_id: child.id.clone(),
            command: "rm -rf build".into(),
            cwd: "/work".into(),
            rule_id: "rm-rf".into(),
            reason: "clean build dir".into(),
        };
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state
            .approvals
            .register(&child.id, "pending-1", req.clone(), tx);

        // The parent session sees nothing pending.
        assert!(state.approvals.pending(&parent.id).is_empty());
        let pending = state.approvals.pending(&child.id);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "pending-1");
        assert_eq!(pending[0].1, req);

        // Completing the approval empties the replay set.
        assert!(state
            .approvals
            .complete(&child.id, "pending-1", Decision::AllowOnce));
        assert!(state.approvals.pending(&child.id).is_empty());
    }
}
