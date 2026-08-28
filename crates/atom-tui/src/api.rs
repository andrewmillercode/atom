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
/// with 409 "session already has an active turn": the TUI believed the
/// session idle, but the server still has a turn registered (a raced
/// pause, a hung tool round, or a stale entry). Callers must never
/// surface this; the recovery is exactly what the message asks for —
/// pause the turn, then dial again.
pub fn is_active_turn_conflict(err: &anyhow::Error) -> bool {
    err.to_string().contains("already has an active turn")
}

/// streamSendHealed dials /send and recovers from the 409
/// active-turn conflict instead of surfacing it: pause every active
/// turn of the session, then retry once. The pause waits server-side
/// until the turn has fully unwound, so the retry starts from a clean
/// idle state. Any other error (or a second 409, meaning the turn
/// would not stop) is returned untouched.
pub async fn stream_send_healed(
    req: &crate::events::SendRequest,
) -> Result<tokio::sync::mpsc::Receiver<Value>> {
    match stream_send(req).await {
        Ok(rx) => Ok(rx),
        Err(e) if is_active_turn_conflict(&e) => {
            let _ = pause_turn(&req.session_id, "").await;
            stream_send(req).await
        }
        Err(e) => Err(e),
    }
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

/// send posts a turn to /send and returns the NDJSON event channel.
pub async fn stream_send(
    req: &crate::events::SendRequest,
) -> Result<tokio::sync::mpsc::Receiver<Value>> {
    atom_server::client::stream_send(&req.session_id, &req.to_body()).await
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
