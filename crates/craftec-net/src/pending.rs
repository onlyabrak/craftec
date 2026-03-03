//! [`PendingFetches`] — rendezvous point for in-flight piece requests.
//!
//! When the repair executor (or any subsystem) needs a piece from the network,
//! it calls [`PendingFetches::register`] to get a `oneshot::Receiver`.  When
//! the inbound message handler receives a `PieceResponse`, it calls
//! [`PendingFetches::resolve`] to deliver the piece to the waiting task.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::oneshot;

use craftec_types::{Cid, CodedPiece};

/// A pending entry wrapping a sender with a registration timestamp.
struct PendingEntry {
    tx: oneshot::Sender<CodedPiece>,
    registered_at: Instant,
}

/// Thread-safe rendezvous map for pending piece fetches.
///
/// Maps each CID to a list of waiting `oneshot::Sender`s.  When a piece arrives,
/// all waiters for that CID are resolved.
pub struct PendingFetches {
    waiters: DashMap<Cid, Vec<PendingEntry>>,
}

impl PendingFetches {
    pub fn new() -> Self {
        Self {
            waiters: DashMap::new(),
        }
    }

    /// Register interest in `cid` and return a receiver that will resolve
    /// when a matching `PieceResponse` arrives.
    pub fn register(&self, cid: &Cid) -> oneshot::Receiver<CodedPiece> {
        let (tx, rx) = oneshot::channel();
        self.waiters.entry(*cid).or_default().push(PendingEntry {
            tx,
            registered_at: Instant::now(),
        });
        tracing::trace!(cid = %cid, "PendingFetches: registered waiter");
        rx
    }

    /// Deliver a piece to all waiting tasks registered for `piece.cid`.
    ///
    /// If multiple waiters are registered, each gets a clone of the piece.
    /// Waiters whose receivers have been dropped are silently skipped.
    pub fn resolve(&self, cid: &Cid, piece: CodedPiece) {
        if let Some((_, entries)) = self.waiters.remove(cid) {
            let count = entries.len();
            for entry in entries {
                let _ = entry.tx.send(piece.clone());
            }
            tracing::debug!(cid = %cid, waiters = count, "PendingFetches: resolved");
        }
    }

    /// Number of CIDs with pending waiters.
    pub fn pending_count(&self) -> usize {
        self.waiters.len()
    }

    /// Total number of pending entries across all CIDs.
    pub fn total_pending(&self) -> usize {
        self.waiters.iter().map(|e| e.value().len()).sum()
    }

    /// Remove stale entries: those whose receivers have been dropped (caller timed out)
    /// or those registered longer than `max_age` ago.
    ///
    /// Returns the number of entries pruned.
    pub fn prune_stale(&self, max_age: Duration) -> usize {
        let now = Instant::now();
        let mut pruned = 0usize;

        // Collect CIDs to avoid borrowing issues during retain.
        let cids: Vec<Cid> = self.waiters.iter().map(|e| *e.key()).collect();

        for cid in cids {
            if let Some(mut entries) = self.waiters.get_mut(&cid) {
                let before = entries.len();
                entries.retain(|entry| {
                    let age_exceeded = now.duration_since(entry.registered_at) >= max_age;
                    let receiver_dropped = entry.tx.is_closed();
                    !age_exceeded && !receiver_dropped
                });
                pruned += before - entries.len();
            }
        }

        // Clean up empty CID entries.
        self.waiters.retain(|_, entries| !entries.is_empty());

        if pruned > 0 {
            tracing::debug!(pruned, "PendingFetches: pruned stale entries");
        }
        pruned
    }
}

impl Default for PendingFetches {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_types::Cid;

    #[tokio::test]
    async fn register_and_resolve() {
        let pending = PendingFetches::new();
        let cid = Cid::from_data(b"test-piece");

        let rx = pending.register(&cid);
        assert_eq!(pending.pending_count(), 1);

        let piece = CodedPiece::new(cid, vec![1], vec![0u8; 16], [0u8; 32]);
        pending.resolve(&cid, piece.clone());

        let received = rx.await.unwrap();
        assert_eq!(received.cid, cid);
        assert_eq!(pending.pending_count(), 0);
    }

    #[tokio::test]
    async fn resolve_unknown_cid_is_noop() {
        let pending = PendingFetches::new();
        let cid = Cid::from_data(b"unknown");
        let piece = CodedPiece::new(cid, vec![1], vec![0u8; 16], [0u8; 32]);
        pending.resolve(&cid, piece);
        assert_eq!(pending.pending_count(), 0);
    }

    #[tokio::test]
    async fn multiple_waiters_all_resolved() {
        let pending = PendingFetches::new();
        let cid = Cid::from_data(b"multi-waiter");

        let rx1 = pending.register(&cid);
        let rx2 = pending.register(&cid);
        assert_eq!(pending.pending_count(), 1);

        let piece = CodedPiece::new(cid, vec![1], vec![42u8; 16], [0u8; 32]);
        pending.resolve(&cid, piece);

        let p1 = rx1.await.unwrap();
        let p2 = rx2.await.unwrap();
        assert_eq!(p1.cid, cid);
        assert_eq!(p2.cid, cid);
    }

    #[test]
    fn dropped_receiver_does_not_panic() {
        let pending = PendingFetches::new();
        let cid = Cid::from_data(b"dropped");

        let _rx = pending.register(&cid);
        drop(_rx);

        let piece = CodedPiece::new(cid, vec![1], vec![0u8; 16], [0u8; 32]);
        pending.resolve(&cid, piece);
    }

    #[test]
    fn prune_stale_removes_closed_receivers() {
        let pending = PendingFetches::new();
        let cid = Cid::from_data(b"prune-closed");

        let rx = pending.register(&cid);
        drop(rx); // receiver dropped → sender is closed

        assert_eq!(pending.total_pending(), 1);
        let pruned = pending.prune_stale(Duration::from_secs(60));
        assert_eq!(pruned, 1);
        assert_eq!(pending.pending_count(), 0);
        assert_eq!(pending.total_pending(), 0);
    }

    #[test]
    fn prune_stale_keeps_active() {
        let pending = PendingFetches::new();
        let cid = Cid::from_data(b"prune-active");

        let _rx = pending.register(&cid); // keep receiver alive
        let pruned = pending.prune_stale(Duration::from_secs(60));
        assert_eq!(pruned, 0);
        assert_eq!(pending.pending_count(), 1);
    }

    #[test]
    fn total_pending_tracks_entries() {
        let pending = PendingFetches::new();
        let c1 = Cid::from_data(b"count1");
        let c2 = Cid::from_data(b"count2");

        let _r1 = pending.register(&c1);
        let _r2 = pending.register(&c1);
        let _r3 = pending.register(&c2);

        assert_eq!(pending.total_pending(), 3);
        assert_eq!(pending.pending_count(), 2); // 2 CIDs
    }
}
