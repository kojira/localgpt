//! In-process turn gate using a tokio Semaphore.
//!
//! Prevents heartbeat and HTTP sessions from running agent turns
//! simultaneously within the same daemon process.

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A single-permit gate that serializes agent turns within a process.
///
/// HTTP handlers call `acquire()` (async, waits for the permit).
/// Heartbeat calls `try_acquire()` and skips if busy.
#[derive(Clone)]
pub struct TurnGate {
    semaphore: Arc<Semaphore>,
}

impl TurnGate {
    pub fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(1)),
        }
    }

    /// Async acquire — waits until the permit is available.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("TurnGate semaphore should never be closed")
    }

    /// Non-blocking try-acquire — returns `None` if an agent turn is in flight.
    pub fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().ok()
    }

    /// Returns `true` if an agent turn is currently in progress.
    pub fn is_busy(&self) -> bool {
        self.semaphore.available_permits() == 0
    }
}

impl Default for TurnGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn is_busy_reflects_permit_state() {
        let gate = TurnGate::new();
        assert!(!gate.is_busy());

        let permit = gate.acquire().await;
        assert!(gate.is_busy());

        drop(permit);
        assert!(!gate.is_busy());
    }

    #[tokio::test]
    async fn try_acquire_returns_none_when_busy() {
        let gate = TurnGate::new();

        let _permit = gate.acquire().await;
        assert!(gate.try_acquire().is_none());
    }

    #[tokio::test]
    async fn try_acquire_succeeds_when_free() {
        let gate = TurnGate::new();
        let permit = gate.try_acquire();
        assert!(permit.is_some());
    }

    #[tokio::test]
    async fn clone_shares_state() {
        let gate1 = TurnGate::new();
        let gate2 = gate1.clone();

        let _permit = gate1.acquire().await;
        assert!(gate2.is_busy());
        assert!(gate2.try_acquire().is_none());
    }
}
