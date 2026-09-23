//! api.rs is the thin facade over atom_server::client so TUI call sites
//! stay small and the socket paths/routes live in one place.

use anyhow::Result;
use serde_json::{json, Value};

use atom_core::providers::providers::Provider;
use atom_core::session::stats::StatsReport;
use atom_core::session::store::{Session, SessionInfo};

pub use atom_server::client::{
    ensure_server, hold_server_alive, is_running, respond_approval, socket_path,
    stop_background_server,
};

/// listSessions fetches all session summaries.
pub async fn list_sessions() -> Result<Vec<SessionInfo>> {
    let v = atom_server::client::get("/api/sessions").await?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

/// getSession fetches the full session with messages.
pub async fn get_session(id: &str) -> Result<Session> {
    let v = atom_server::client::get(&format!("/api/sessions/{id}")).await?;
    Ok(serde_json::from_value(v)?)
}

/// Result of a successful fork: the new child session plus the draft
/// prompt the TUI pre-fills (empty for fork-from-latest).
#[derive(Debug, Clone)]
pub struct ForkedSession {
    pub info: SessionInfo,
    pub draft: String,
}

/// forkSession POSTs /api/sessions/{source_id}/fork and decodes the
/// response into the new session + draft. `position = None` means
/// "fork from latest" (full transcript, empty draft). `Some(n)`
/// truncates the transcript to the first n messages.
pub async fn fork_session(source_id: &str, position: Option<i64>) -> Result<ForkedSession> {
    let body = json!({ "position": position });
    let v = atom_server::client::post(&format!("/api/sessions/{source_id}/fork"), &body).await?;
    // The server wraps the child SessionInfo as `{"info": ..., "draft": ...}`.
    // Deserialize `info` rather than the whole envelope, otherwise the
    // SessionInfo's required `created_at`/`updated_at` fields are missing
    // and serde fails with "missing field created_at".
    let info: SessionInfo = serde_json::from_value(v.get("info").cloned().unwrap_or(Value::Null))?;
    let draft = v
        .get("draft")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    Ok(ForkedSession { info, draft })
}

/// listChildren fetches child sessions of a dispatch parent.
pub async fn list_children(id: &str) -> Result<Vec<SessionInfo>> {
    let v = atom_server::client::get(&format!("/api/sessions/{id}/children")).await?;
    Ok(serde_json::from_value(v).unwrap_or_else(|_| Vec::new()))
}

/// createSession POSTs a fresh session.
pub async fn create_session(
    provider: &str,
    model: &str,
    cwd: &str,
    thinking: &str,
) -> Result<SessionInfo> {
    let mut body = json!({"provider": provider, "model": model, "cwd": cwd});
    if !thinking.is_empty() {
        body["thinking"] = json!(thinking);
    }
    let v = atom_server::client::post("/api/sessions", &body).await?;
    Ok(serde_json::from_value(v)?)
}

/// patchSessionModel updates model/thinking mid-session.
pub async fn patch_session_model(
    id: &str,
    provider: &str,
    model: &str,
    thinking: &str,
) -> Result<Value> {
    let body = json!({"provider": provider, "model": model, "thinking": thinking});
    atom_server::client::patch(&format!("/api/sessions/{id}"), &body).await
}

/// patchSessionThinking updates only the reasoning level.
pub async fn patch_session_thinking(id: &str, thinking: &str) -> Result<Value> {
    let body = json!({"thinking": thinking});
    atom_server::client::patch(&format!("/api/sessions/{id}"), &body).await
}

/// patchSessionCwd moves the session's working directory (shell mode `cd`).
pub async fn patch_session_cwd(id: &str, cwd: &str) -> Result<Value> {
    let body = json!({"cwd": cwd});
    atom_server::client::patch(&format!("/api/sessions/{id}"), &body).await
}

/// deleteSession removes a session permanently.
pub async fn delete_session(id: &str) -> Result<Value> {
    atom_server::client::delete(&format!("/api/sessions/{id}")).await
}

/// pauseTurn asks the server to stop an active stream. An empty turn_id
/// pauses every active turn of the session.
pub async fn pause_turn(id: &str, turn_id: &str) -> Result<()> {
    let body = json!({ "turn_id": turn_id });
    atom_server::client::post(&format!("/api/sessions/{id}/pause"), &body).await?;
    Ok(())
}

/// isActiveTurnConflict reports whether a failed /send dial was rejected
/// with 409 "session already has an active turn". With mid-turn
/// injection the server no longer answers a live turn with 409 (it
/// queues the prompt instead), so this can only fire against an older
/// server. Kept for clients that classify dial errors.
pub fn is_active_turn_conflict(err: &anyhow::Error) -> bool {
    err.to_string().contains("already has an active turn")
}

/// send posts a turn to /send and returns the NDJSON event channel.
/// When a turn is already active the server injects the prompt into it
/// and answers with a tiny {"type":"injected"} stream that closes, so
/// this never pauses anything and never sees a 409.
pub async fn stream_send(
    req: &crate::events::SendRequest,
) -> Result<tokio::sync::mpsc::Receiver<Value>> {
    atom_server::client::stream_send(&req.session_id, &req.to_body()).await
}

/// compact folds history on an in-flight turn.
pub async fn compact(id: &str, instructions: &str) -> Result<()> {
    let body = json!({ "instructions": instructions });
    atom_server::client::post(&format!("/api/sessions/{id}/compact"), &body).await?;
    Ok(())
}

/// fetchStatsReport pulls the aggregated usage report.
pub async fn fetch_stats_report(days: i64) -> Result<StatsReport> {
    let path = if days > 0 {
        format!("/api/stats?days={days}")
    } else {
        "/api/stats".to_string()
    };
    let v = atom_server::client::get(&path).await?;
    Ok(serde_json::from_value(v)?)
}

/// subscribe opens the /events NDJSON channel.
pub async fn stream_events(id: &str) -> Result<tokio::sync::mpsc::Receiver<Value>> {
    atom_server::client::stream_events(id).await
}

/// fetchAllModels fetches models from all providers concurrently and
/// returns them sorted by provider name then model name.
pub async fn fetch_all_models(providers: &[Provider]) -> Vec<(Provider, String)> {
    use futures::FutureExt;
    let futures = providers.iter().map(|p| {
        let p = p.clone();
        async move {
            let models = atom_core::providers::providers::fetch_models(&p)
                .await
                .unwrap_or_default();
            (p, models)
        }
        .boxed()
    });
    let results = futures::future::join_all(futures).await;
    let mut entries: Vec<(Provider, String)> = Vec::new();
    for (p, models) in results {
        for m in models {
            entries.push((p.clone(), m));
        }
    }
    entries.sort_by(|a, b| (&a.0.name, &a.1).cmp(&(&b.0.name, &b.1)));
    entries
}
