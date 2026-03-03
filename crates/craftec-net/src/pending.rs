//! [`PendingFetches`] — rendezvous point for in-flight piece requests.
//!
//! When the repair executor (or any subsystem) needs a piece from the network,
//! it calls [`PendingFetches::register`] to get a `oneshot::Receiver`.  When
//! the inbound message handler receives a `PieceResponse`, it calls
//! [`PendingFetches::resolve`] to deliver the piece to the waiting task.

use dashmap::DashMap;
use tokio::sync::oneshot;

use craftec_types::{Cid, CodedPiece};

/// Thread-safe rendezvous map for pending piece fetches.
///
/// Maps each CID to a list of waiting `oneshot::Sender`s.  When a piece arrives,
/// all waiters for that CID are resolved.
pub struct PendingFetches {
    waiters: DashMap<Cid, Vec<oneshot::Sender<CodedPiece>>>,
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
        self.waiters.entry(*cid).or_default().push(tx);
        tracing::trace!(cid = %cid, "PendingFetches: registered waiter");
        rx
    }

    /// Deliver a piece to all waiting tasks registered for `piece.cid`.
    ///
    /// If multiple waiters are registered, each gets a clone of the piece.
    /// Waiters whose receivers have been dropped are silently skipped.
    pub fn resolve(&self, cid: &Cid, piece: CodedPiece) {
        if let Some((_, senders)) = self.waiters.remove(cid) {
            let count = senders.len();
            for tx in senders {
                let _ = tx.send(piece.clone());
            }
            tracing::debug!(cid = %cid, waiters = count, "PendingFetches: resolved");
        }
    }

    /// Number of CIDs with pending waiters.
    pub fn pending_count(&self) -> usize {
        self.waiters.len()
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
}
