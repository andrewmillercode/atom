//! A minimal cancellation token (tokio-util's CancellationToken is not in
//! the workspace dependency set). Mirrors the Go turn code's use of
//! context cancellation: `cancel` wakes every waiter, `cancelled()`
//! resolves immediately when already cancelled.
//!
//! Lives in atom-core so both the server (turn loop) and the tool
//! layer (atom-sandbox / atom-tools) can share one token: cancelling a
//! turn reaches in-flight tool execution, not just the model stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Default)]
struct Inner {
    flag: AtomicBool,
    notify: Notify,
}

#[derive(Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled (immediately if already cancelled).
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.0.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_when_cancelled_before_await() {
        let t = CancelToken::new();
        t.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(50), t.cancelled())
            .await
            .expect("already cancelled should resolve");
    }

    #[tokio::test]
    async fn wakes_waiters_on_cancel() {
        let t = CancelToken::new();
        let t2 = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            t2.cancel();
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), t.cancelled())
            .await
            .expect("waiter should be woken");
    }
}
