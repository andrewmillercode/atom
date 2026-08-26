//! Shared server state: per-session event pub/sub, connection tracking
//! with the idle-shutdown monitor, the turn table (start/pause/compact
//! queueing), and pending sandbox approvals. Ported from server.go.

use crate::cancel::CancelToken;
use atom_core::session::store::SessionStore;
use atom_sandbox::approvals::{ApprovalRequest, Decision};
use atom_sandbox::policy::SandboxConfig;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};

/// idleShutdownAfter: the server exits when no client connections have
/// been open for this long. Every running TUI holds a long-lived /events
/// subscription, so a visible atom instance keeps the server alive; once
/// the last instance quits, the server cleans up after itself.
pub const IDLE_SHUTDOWN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Event pub/sub.
// ---------------------------------------------------------------------------

type EventTx = mpsc::Sender<Value>;

/// sessionSubs maps session IDs to their active subscriber channels.
/// When handleSend processes a turn, it broadcasts each event to all
/// subscribers so other atom instances viewing the same session see
/// updates in real time.
#[derive(Default)]
pub struct SessionSubs {
    next_id: AtomicU64,
    subs: Mutex<HashMap<String, Vec<(u64, EventTx)>>>,
}

pub struct Subscriber {
    pub id: u64,
    pub rx: mpsc::Receiver<Value>,
}

impl SessionSubs {
    /// subscribeSession registers a new subscriber for a session and
    /// returns the channel events will be sent to (buffered like Go's
    /// chan of 64).
    pub fn subscribe(&self, id: &str) -> Subscriber {
        let (tx, rx) = mpsc::channel(64);
        let sub_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.subs
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .push((sub_id, tx));
        Subscriber { id: sub_id, rx }
    }

    /// unsubscribeSession removes a subscriber channel from a session.
    /// Returns true when the last subscriber left (the caller then
    /// cancels the session's active turns so generation doesn't keep
    /// running with no client listening).
    pub fn unsubscribe(&self, id: &str, sub_id: u64) -> bool {
        let mut subs = self.subs.lock().unwrap();
        let Some(list) = subs.get_mut(id) else {
            return false;
        };
        list.retain(|(sid, _)| *sid != sub_id);
        if list.is_empty() {
            subs.remove(id);
            return true;
        }
        false
    }

    /// broadcastSession sends an event to all subscribers of a session.
    /// It never blocks. A lagging subscriber is disconnected instead of
    /// silently losing terminal events; the client reconnects and reloads.
    pub fn broadcast(&self, id: &str, event: &Value) {
        let mut subs = self.subs.lock().unwrap();
        let Some(list) = subs.get_mut(id) else {
            return;
        };
        list.retain(|(_, tx)| tx.try_send(event.clone()).is_ok());
        if list.is_empty() {
            subs.remove(id);
        }
    }

    #[cfg(test)]
    pub fn subscriber_count(&self, id: &str) -> usize {
        self.subs
            .lock()
            .unwrap()
            .get(id)
            .map(|l| l.len())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Connection tracking + idle monitor.
// ---------------------------------------------------------------------------

/// Counts in-flight requests (including long-lived /events and
/// /keepalive streams) and records when the count last dropped to zero,
/// mirroring Go's activeConns/idleSince pair.
pub struct ConnTracker {
    active: AtomicI64,
    idle_since: Mutex<Option<std::time::Instant>>,
    pub idle_after: std::time::Duration,
}

impl Default for ConnTracker {
    fn default() -> Self {
        ConnTracker {
            active: AtomicI64::new(0),
            idle_since: Mutex::new(None),
            idle_after: IDLE_SHUTDOWN_AFTER,
        }
    }
}

impl ConnTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the idle countdown now; the first connection pauses it.
    pub fn start_idle_clock(&self) {
        *self.idle_since.lock().unwrap() = Some(std::time::Instant::now());
    }

    /// noteConnOpen marks a connection as open, pausing the idle countdown.
    pub fn note_open(&self) {
        self.active.fetch_add(1, Ordering::SeqCst);
    }

    /// noteConnClosed marks a connection as closed. When the last
    /// connection drops, the idle countdown starts.
    pub fn note_closed(&self) {
        if self.active.fetch_add(-1, Ordering::SeqCst) == 1 {
            *self.idle_since.lock().unwrap() = Some(std::time::Instant::now());
        }
    }

    pub fn active_conns(&self) -> i64 {
        self.active.load(Ordering::SeqCst)
    }

    /// True when zero connections have been open for at least
    /// `idle_after`. The idle monitor exits the server when this fires.
    pub fn idle_expired(&self) -> bool {
        if self.active_conns() != 0 {
            return false;
        }
        let since = *self.idle_since.lock().unwrap();
        match since {
            Some(t) => t.elapsed() >= self.idle_after,
            None => false,
        }
    }
}

/// RAII guard pairing note_open/note_closed around each request (Go's
/// connTracker middleware). Held for the whole lifetime of streaming
/// responses by moving it into the response worker task.
pub struct ConnGuard(Arc<ConnTracker>);

impl ConnGuard {
    pub fn take(tracker: &Arc<ConnTracker>) -> Self {
        tracker.note_open();
        ConnGuard(tracker.clone())
    }

    pub fn shared(&self) -> Arc<ConnTracker> {
        self.0.clone()
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.note_closed();
    }
}

// ---------------------------------------------------------------------------
// Turn table.
// ---------------------------------------------------------------------------

struct RoundState {
    round_cancel: Option<CancelToken>,
    compact_queued: bool,
    compact_instr: String,
}

/// One registered turn: its ID, the whole-turn cancel token (Esc /
/// pause / last-subscriber-left), and the current provider request's
/// cancel token so /compact can interrupt generation without ending
/// the turn. The next loop iteration then folds history and resumes.
pub struct TurnHandle {
    pub turn_id: String,
    cancel: CancelToken,
    round: Mutex<RoundState>,
}

impl TurnHandle {
    pub fn set_round_cancel(&self, c: Option<CancelToken>) {
        self.round.lock().unwrap().round_cancel = c;
    }

    pub fn cancel_round(&self) {
        if let Some(c) = &self.round.lock().unwrap().round_cancel {
            c.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// queueCompact asks the in-flight turn to fold history. If a model
    /// request is streaming, it is cancelled so the loop can compact and
    /// resume; the turn itself stays alive.
    pub fn queue_compact(&self, instructions: &str) {
        {
            let mut r = self.round.lock().unwrap();
            r.compact_queued = true;
            r.compact_instr = instructions.to_string();
        }
        self.cancel_round();
    }

    pub fn take_compact(&self) -> Option<String> {
        let mut r = self.round.lock().unwrap();
        if !r.compact_queued {
            return None;
        }
        r.compact_queued = false;
        Some(std::mem::take(&mut r.compact_instr))
    }
}

#[derive(Default)]
struct TurnMaps {
    turns: HashMap<String, Vec<Arc<TurnHandle>>>,
    reserved: std::collections::HashSet<String>,
    pending_pauses: HashMap<String, Vec<String>>,
    /// Sessions whose most recently prepared turn reached end_turn. This
    /// closes the race where a detached dispatch turn starts and finishes
    /// between result(wait:true) polls.
    completed: std::collections::HashSet<String>,
    /// Notified (one permit) whenever end_turn removes a turn, so a
    /// caller blocked in wait_idle wakes to re-check whether the session
    /// is idle. Wrapped in Arc so wait_idle can clone the handle out of
    /// the lock and register interest without holding the Mutex.
    notify: Arc<Notify>,
}

/// Registry of each session's active turns plus pauses that raced ahead
/// of their turn (pendingPauses).
#[derive(Default)]
pub struct TurnTable(Mutex<TurnMaps>);

impl TurnTable {
    /// startTurn registers a cancellable context for a session's turn
    /// and returns it. If a pause for this turn arrived before the turn
    /// registered, the context is cancelled immediately.
    pub fn start_turn(&self, id: &str, turn_id: &str) -> Arc<TurnHandle> {
        let handle = Arc::new(TurnHandle {
            turn_id: turn_id.to_string(),
            cancel: CancelToken::new(),
            round: Mutex::new(RoundState {
                round_cancel: None,
                compact_queued: false,
                compact_instr: String::new(),
            }),
        });
        let mut maps = self.0.lock().unwrap();
        maps.reserved.remove(id);
        maps.completed.remove(id);
        if let Some(pauses) = maps.pending_pauses.get_mut(id) {
            if let Some(pos) = pauses.iter().position(|p| p == turn_id) {
                pauses.remove(pos);
                handle.cancel.cancel();
            }
            if pauses.is_empty() {
                maps.pending_pauses.remove(id);
            }
        }
        maps.turns
            .entry(id.to_string())
            .or_default()
            .push(handle.clone());
        handle
    }

    /// requestSessionCompact queues compaction on the session's latest
    /// turn. Returns false when no turn is running.
    pub fn request_session_compact(&self, id: &str, instructions: &str) -> bool {
        let turns = self.0.lock().unwrap().turns.get(id).cloned();
        match turns {
            Some(t) if !t.is_empty() => {
                t[t.len() - 1].queue_compact(instructions);
                true
            }
            _ => false,
        }
    }

    /// endTurn removes a finished turn from the registry.
    pub fn end_turn(&self, id: &str, handle: &Arc<TurnHandle>) {
        let mut maps = self.0.lock().unwrap();
        if let Some(turns) = maps.turns.get_mut(id) {
            if let Some(pos) = turns.iter().position(|t| Arc::ptr_eq(t, handle)) {
                turns.remove(pos);
            }
            if turns.is_empty() {
                maps.turns.remove(id);
                maps.completed.insert(id.to_string());
            }
        }
        maps.notify.notify_one();
    }

    /// cancelSessionTurns cancels every active turn for a session. Used
    /// when the last subscriber leaves so generation doesn't keep
    /// running with no client listening. Pending pauses are cleared too.
    pub fn cancel_session_turns(&self, id: &str) {
        let turns = {
            let mut maps = self.0.lock().unwrap();
            let turns = maps.turns.get(id).cloned().unwrap_or_default();
            maps.pending_pauses.remove(id);
            turns
        };
        for t in turns {
            t.cancel.cancel();
        }
    }

    /// pauseSession cancels the active turn with the given turn ID for a
    /// session. If the turn hasn't registered yet (the pause raced ahead
    /// of the send), the pause is remembered and applied when the turn
    /// starts. An empty turn_id cancels every turn of the session.
    pub fn pause_session(&self, id: &str, turn_id: &str) {
        let mut maps = self.0.lock().unwrap();
        let mut cancelled = false;
        if let Some(turns) = maps.turns.get(id) {
            for t in turns {
                if turn_id.is_empty() || t.turn_id == turn_id {
                    t.cancel.cancel();
                    cancelled = true;
                }
            }
        }
        if !cancelled && !turn_id.is_empty() {
            maps.pending_pauses
                .entry(id.to_string())
                .or_default()
                .push(turn_id.to_string());
        }
    }

    pub fn session_has_active_turn(&self, id: &str) -> bool {
        let maps = self.0.lock().unwrap();
        maps.reserved.contains(id) || maps.turns.get(id).map(|t| !t.is_empty()).unwrap_or(false)
    }

    /// Block (up to `timeout`) until the session has no active turn —
    /// i.e. the prior turn has fully unwound and reached end_turn, so a
    /// follow-up /send won't race it. Returns true if idle, false on
    /// timeout. Uses notify_one() permits so a wakeup that fires between
    /// the active-turn check and registering interest is not lost.
    pub async fn wait_idle(&self, id: &str, timeout: std::time::Duration) -> bool {
        let notify = self.0.lock().unwrap().notify.clone();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !self.session_has_active_turn(id) {
                return true;
            }
            let Some(remain) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return !self.session_has_active_turn(id);
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            tokio::select! {
                _ = &mut notified => continue,
                _ = tokio::time::sleep(remain) => return !self.session_has_active_turn(id),
            }
        }
    }

    pub fn session_turn_completed(&self, id: &str) -> bool {
        self.0.lock().unwrap().completed.contains(id)
    }

    /// Marks a turn as pending before its detached task is scheduled. This
    /// prevents result(wait:true) from treating a previous completion as the
    /// completion of a just-started follow-up.
    pub fn prepare_session_turn(&self, id: &str) {
        let mut maps = self.0.lock().unwrap();
        maps.completed.remove(id);
        maps.reserved.insert(id.to_string());
    }

    /// Atomically reserves an idle session before its detached turn task is
    /// spawned. This closes the gap where two sends could load independent
    /// session snapshots before either registered an active TurnHandle.
    pub fn try_prepare_session_turn(&self, id: &str) -> bool {
        let mut maps = self.0.lock().unwrap();
        if maps.reserved.contains(id)
            || maps
                .turns
                .get(id)
                .map(|turns| !turns.is_empty())
                .unwrap_or(false)
        {
            return false;
        }
        maps.completed.remove(id);
        maps.reserved.insert(id.to_string());
        true
    }

    pub fn clear_pending_pauses(&self, id: &str) {
        self.0.lock().unwrap().pending_pauses.remove(id);
    }
}

// ---------------------------------------------------------------------------
// Pending approvals.
// ---------------------------------------------------------------------------

/// One sandbox approval prompt awaiting POST /approval/:session: the
/// request details (so the prompt can be replayed to late subscribers)
/// plus the one-shot decision channel.
struct PendingApproval {
    req: ApprovalRequest,
    tx: oneshot::Sender<Decision>,
}

/// Sandbox approval prompts awaiting POST /approval/:session. Keyed by
/// (session_id, approval id).
#[derive(Default)]
pub struct PendingApprovals(Mutex<HashMap<(String, String), PendingApproval>>);

impl PendingApprovals {
    /// Registers a waiter; any previous waiter for the same key is
    /// dropped (which denies it).
    pub fn register(
        &self,
        session_id: &str,
        id: &str,
        req: ApprovalRequest,
        tx: oneshot::Sender<Decision>,
    ) {
        self.0.lock().unwrap().insert(
            (session_id.to_string(), id.to_string()),
            PendingApproval { req, tx },
        );
    }

    /// Completes a pending approval with the user's decision. Returns
    /// false when no matching prompt exists.
    pub fn complete(&self, session_id: &str, id: &str, decision: Decision) -> bool {
        match self
            .0
            .lock()
            .unwrap()
            .remove(&(session_id.to_string(), id.to_string()))
        {
            Some(pending) => pending.tx.send(decision).is_ok(),
            None => false,
        }
    }

    pub fn remove(&self, session_id: &str, id: &str) {
        self.0
            .lock()
            .unwrap()
            .remove(&(session_id.to_string(), id.to_string()));
    }

    pub fn has_pending(&self, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .keys()
            .any(|(sid, _)| sid == session_id)
    }

    /// pending returns a snapshot of every prompt still awaiting a
    /// decision for a session, newest registration last. Used to replay
    /// `approval_request` events to viewers that subscribe after the
    /// original broadcast (e.g. navigating into a subagent that is
    /// blocked on the approval gate).
    pub fn pending(&self, session_id: &str) -> Vec<(String, ApprovalRequest)> {
        let mut out: Vec<(String, ApprovalRequest)> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|((sid, _), _)| sid == session_id)
            .map(|((_, id), pending)| (id.clone(), pending.req.clone()))
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out
    }
}

// ---------------------------------------------------------------------------
// Application state.
// ---------------------------------------------------------------------------

pub struct AppState {
    pub store: Arc<SessionStore>,
    store_io: Arc<Semaphore>,
    pub subs: SessionSubs,
    pub turns: TurnTable,
    pub approvals: PendingApprovals,
    pub cfg: SandboxConfig,
    pub tracker: Arc<ConnTracker>,
    /// Per-session seen-file caches handed to ToolCtx (Go keeps fileSeen
    /// on the shared Session struct).
    pub files: Mutex<HashMap<String, Arc<atom_tools::FileSeen>>>,
}

impl AppState {
    pub fn new(store: Arc<SessionStore>, cfg: SandboxConfig, tracker: Arc<ConnTracker>) -> Self {
        store.reconcile_delegate_statuses();
        AppState {
            store,
            store_io: Arc::new(Semaphore::new(1)),
            subs: SessionSubs::default(),
            turns: TurnTable::default(),
            approvals: PendingApprovals::default(),
            cfg,
            tracker,
            files: Mutex::new(HashMap::new()),
        }
    }

    /// Runs synchronous SQLite/session work away from Tokio's workers.
    /// The permit is moved into the blocking task so cancellation cannot
    /// release it while the underlying operation is still running.
    pub async fn store_call<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&SessionStore) -> T + Send + 'static,
    {
        let permit = self
            .store_io
            .clone()
            .acquire_owned()
            .await
            .expect("session store semaphore closed");
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f(&store)
        })
        .await
        .expect("session store task panicked")
    }

    /// Get-or-create the FileSeen cache for a session.
    pub fn file_seen_for(&self, session_id: &str) -> Arc<atom_tools::FileSeen> {
        self.files
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(atom_tools::FileSeen::new()))
            .clone()
    }

    /// Remove the seen-file cache when its session is deleted.
    pub fn remove_file_seen(&self, session_id: &str) {
        self.files.lock().unwrap().remove(session_id);
    }
}

/// Only unlink the socket if nobody else is listening on the path;
/// otherwise a live server would be left with an unreachable listener
/// (its clients see "no such file or directory").
pub fn unlink_socket_if_no_listener(path: &Path) {
    match std::os::unix::net::UnixStream::connect(path) {
        Err(_) => {
            let _ = std::fs::remove_file(path);
        }
        Ok(conn) => drop(conn),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn conn_tracker_idle_semantics() {
        let tracker = Arc::new(ConnTracker {
            idle_after: Duration::from_millis(30),
            ..Default::default()
        });
        assert!(!tracker.idle_expired(), "no clock started yet");
        tracker.start_idle_clock();
        assert!(!tracker.idle_expired(), "within window");
        std::thread::sleep(Duration::from_millis(40));
        assert!(tracker.idle_expired(), "window elapsed with zero conns");

        // A live connection suppresses expiry regardless of the clock.
        let guard = ConnGuard::take(&tracker);
        std::thread::sleep(Duration::from_millis(40));
        assert!(!tracker.idle_expired(), "open connection must pause idle");

        // Closing the last connection restarts the countdown.
        drop(guard);
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            tracker.idle_expired(),
            "countdown restarted after last close"
        );
    }

    #[test]
    fn turn_table_pending_pause_applies_to_late_turn() {
        let tt = TurnTable::default();
        // Pause races ahead of the send: remembered, not cancelled.
        tt.pause_session("s1", "t1");
        assert!(!tt.session_has_active_turn("s1"));
        let h = tt.start_turn("s1", "t1");
        assert!(h.is_cancelled(), "pending pause must cancel on start");
        tt.end_turn("s1", &h);

        // Empty turn_id pauses every active turn.
        let a = tt.start_turn("s2", "a");
        let b = tt.start_turn("s2", "b");
        tt.pause_session("s2", "");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(tt.session_has_active_turn("s2"));
        tt.end_turn("s2", &a);
        tt.end_turn("s2", &b);
        assert!(!tt.session_has_active_turn("s2"));

        // Non-matching id records another pending pause.
        let c = tt.start_turn("s3", "real");
        tt.pause_session("s3", "dispatch-s3");
        assert!(!c.is_cancelled());
        let late = tt.start_turn("s3", "dispatch-s3");
        assert!(
            late.is_cancelled(),
            "dispatch pause quirk: late dispatch turn dies immediately"
        );
    }

    #[test]
    fn turn_table_remembers_fast_detached_completion() {
        let tt = TurnTable::default();
        tt.prepare_session_turn("child");
        assert!(!tt.session_turn_completed("child"));

        let h = tt.start_turn("child", "dispatch-child");
        tt.end_turn("child", &h);

        assert!(!tt.session_has_active_turn("child"));
        assert!(tt.session_turn_completed("child"));

        tt.prepare_session_turn("child");
        assert!(!tt.session_turn_completed("child"));
    }

    #[test]
    fn turn_table_reserves_only_one_pending_turn_per_session() {
        let tt = TurnTable::default();
        assert!(tt.try_prepare_session_turn("s"));
        assert!(!tt.try_prepare_session_turn("s"));
        assert!(tt.session_has_active_turn("s"));

        let handle = tt.start_turn("s", "t");
        assert!(tt.session_has_active_turn("s"));
        assert!(!tt.try_prepare_session_turn("s"));

        tt.end_turn("s", &handle);
        assert!(tt.try_prepare_session_turn("s"));
    }

    #[tokio::test]
    async fn wait_idle_returns_when_turn_ends() {
        let tt = Arc::new(TurnTable::default());
        let h = tt.start_turn("s", "t");
        assert!(tt.session_has_active_turn("s"));

        let tt2 = tt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tt2.end_turn("s", &h);
        });

        let res = tokio::time::timeout(
            Duration::from_secs(1),
            tt.wait_idle("s", Duration::from_secs(2)),
        )
        .await;
        assert!(
            res.is_ok(),
            "wait_idle should return well before the timeout"
        );
        assert!(
            res.unwrap(),
            "wait_idle should report idle once the turn ends"
        );
        assert!(!tt.session_has_active_turn("s"));
    }

    #[tokio::test]
    async fn wait_idle_times_out_for_never_ending_turn() {
        let tt = TurnTable::default();
        let _h = tt.start_turn("x", "t");
        assert!(tt.session_has_active_turn("x"));

        let start = std::time::Instant::now();
        let idle = tt.wait_idle("x", Duration::from_millis(100)).await;
        let elapsed = start.elapsed();
        assert!(!idle, "never-ending turn must not report idle");
        assert!(
            elapsed >= Duration::from_millis(90),
            "wait_idle should block close to the timeout, took {elapsed:?}"
        );
        assert!(tt.session_has_active_turn("x"));
    }

    #[test]
    fn compact_queueing_interrupts_round() {
        let tt = TurnTable::default();
        assert!(!tt.request_session_compact("x", "focus"), "no turn running");
        let h = tt.start_turn("x", "t");
        assert!(tt.request_session_compact("x", "focus"));
        assert_eq!(h.take_compact().as_deref(), Some("focus"));
        assert_eq!(h.take_compact(), None);
        tt.end_turn("x", &h);
    }

    #[test]
    fn subs_broadcast_and_unsubscribe() {
        let subs = SessionSubs::default();
        let mut s1 = subs.subscribe("sess");
        assert_eq!(subs.subscriber_count("sess"), 1);
        subs.broadcast("sess", &serde_json::json!({"type": "saved"}));
        let got = s1.rx.try_recv().unwrap();
        assert_eq!(got["type"], "saved");

        let s3 = subs.subscribe("sess");
        // Another subscriber remains, so this is not the last.
        assert!(!subs.unsubscribe("sess", s1.id));
        assert!(
            subs.unsubscribe("sess", s3.id),
            "last subscriber reports true"
        );
        assert_eq!(subs.subscriber_count("sess"), 0);
        assert!(!subs.unsubscribe("sess", 999));
    }

    #[test]
    fn lagging_subscriber_is_disconnected_instead_of_losing_events() {
        let subs = SessionSubs::default();
        let mut subscriber = subs.subscribe("sess");
        for i in 0..64 {
            subs.broadcast("sess", &serde_json::json!({"seq": i}));
        }
        assert_eq!(subs.subscriber_count("sess"), 1);

        subs.broadcast("sess", &serde_json::json!({"type": "done"}));
        assert_eq!(subs.subscriber_count("sess"), 0);
        for _ in 0..64 {
            subscriber.rx.try_recv().unwrap();
        }
        assert!(matches!(
            subscriber.rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn store_call_keeps_runtime_workers_available() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::new(
            Arc::new(SessionStore::open_in_dir(dir.path()).unwrap()),
            SandboxConfig::default(),
            Arc::new(ConnTracker::default()),
        ));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let worker = tokio::spawn(async move {
            worker_state
                .store_call(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .await;
        });
        entered_rx.await.unwrap();

        let heartbeat = tokio::time::timeout(
            Duration::from_millis(250),
            tokio::time::sleep(Duration::from_millis(10)),
        )
        .await;
        release_tx.send(()).unwrap();
        worker.await.unwrap();
        assert!(heartbeat.is_ok(), "blocking store work starved Tokio");
    }

    #[test]
    fn remove_file_seen_drops_session_cache() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            Arc::new(SessionStore::open_in_dir(dir.path()).unwrap()),
            SandboxConfig::default(),
            Arc::new(ConnTracker::default()),
        );
        let seen = state.file_seen_for("session");
        assert_eq!(state.files.lock().unwrap().len(), 1);
        state.remove_file_seen("session");
        assert!(state.files.lock().unwrap().is_empty());
        assert!(!Arc::ptr_eq(&seen, &state.file_seen_for("session")));
    }
}
