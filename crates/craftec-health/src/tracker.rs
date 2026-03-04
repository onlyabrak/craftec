//! [`PieceTracker`] — live piece availability map for health monitoring.
//!
//! The tracker maintains a concurrent map of `Cid → Vec<PieceHolder>` recording
//! which nodes currently hold coded pieces for each known CID.  This map is
//! the primary data source for [`HealthScanner`] when determining whether a CID
//! has enough redundancy.
//!
//! # RLNC semantics
//!
//! Unlike Reed-Solomon, RLNC coded pieces are unique (random GF(2⁸) coefficient
//! vectors), so there are no "piece indices".  The tracker records one entry per
//! node per CID, tracking how many pieces each node holds.
//!
//! # Update protocol
//!
//! Nodes announce their holdings via the Craftec wire protocol.  The net layer
//! ingests these announcements and calls [`PieceTracker::record_piece`].  When
//! SWIM declares a node dead, [`PieceTracker::remove_node`] purges all its records.
//!
//! # Staleness
//!
//! [`PieceTracker::prune_stale`] removes records whose `last_seen` timestamp is
//! older than `max_age`.  Call this periodically from a maintenance task.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use craftec_types::{Cid, NodeId};

// ── PieceHolder ─────────────────────────────────────────────────────────────

/// A record indicating that `node_id` holds coded pieces for some CID.
///
/// RLNC pieces have no indices — each coded piece is unique (random GF(2⁸)
/// coefficient vector).  We track `piece_count` (how many pieces this node
/// holds) rather than individual piece indices.
#[derive(Debug, Clone)]
pub struct PieceHolder {
    /// The node that holds pieces for this CID.
    pub node_id: NodeId,
    /// Number of coded pieces this node holds for this CID.
    pub piece_count: u32,
    /// Wall-clock time when this holder record was last confirmed (via ping or announcement).
    pub last_seen: Instant,
}

// ── PieceTracker ────────────────────────────────────────────────────────────

/// Concurrent, lock-free piece availability tracker.
///
/// Cheap to clone — all clones share the same underlying map via `Arc`.
#[derive(Clone, Default)]
pub struct PieceTracker {
    /// `Cid` → list of holder records (one per node).
    availability: Arc<DashMap<Cid, Vec<PieceHolder>>>,
    /// `Cid` → K value used for RLNC encoding (C7: variable-K support).
    /// First-writer-wins: once set, the K value for a CID is immutable.
    k_values: Arc<DashMap<Cid, u32>>,
}

impl PieceTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        tracing::debug!("PieceTracker: initialised");
        Self::default()
    }

    /// Record that `holder.node_id` holds pieces for `cid`.
    ///
    /// If a record for the same `node_id` already exists, the `piece_count`
    /// is updated to the max of the existing and new values, and `last_seen`
    /// is refreshed.
    pub fn record_piece(&self, cid: &Cid, holder: PieceHolder) {
        tracing::trace!(
            cid = %cid,
            node = %holder.node_id,
            count = holder.piece_count,
            "PieceTracker: recording piece holder"
        );
        let mut entry = self.availability.entry(*cid).or_default();
        // Update existing record if node_id matches.
        if let Some(existing) = entry.iter_mut().find(|h| h.node_id == holder.node_id) {
            existing.piece_count = existing.piece_count.max(holder.piece_count);
            existing.last_seen = holder.last_seen;
        } else {
            entry.push(holder);
        }
    }

    /// Return all [`PieceHolder`] records for `cid`.
    ///
    /// Returns an empty `Vec` if no records exist for `cid`.
    pub fn get_holders(&self, cid: &Cid) -> Vec<PieceHolder> {
        self.availability
            .get(cid)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Return the total number of coded pieces available across all nodes for `cid`.
    ///
    /// In RLNC each piece is unique (random GF(2⁸) coefficient vector), so the
    /// total is the sum of each holder's `piece_count`.  This is compared against
    /// `k` (minimum pieces to reconstruct) and `target` (desired redundancy).
    pub fn available_count(&self, cid: &Cid) -> u32 {
        self.availability
            .get(cid)
            .map(|holders| holders.iter().map(|h| h.piece_count).sum())
            .unwrap_or(0)
    }

    /// Return how many coded pieces `node_id` holds locally for `cid`.
    ///
    /// Returns 0 if the node has no records for this CID.
    pub fn local_piece_count(&self, cid: &Cid, node_id: &NodeId) -> u32 {
        self.availability
            .get(cid)
            .and_then(|holders| {
                holders
                    .iter()
                    .find(|h| &h.node_id == node_id)
                    .map(|h| h.piece_count)
            })
            .unwrap_or(0)
    }

    /// Return each holder with their piece count for `cid`.
    ///
    /// Used by the repair executor to prioritise distribution targets:
    /// peers with 1 piece (bring to ≥2 for repair eligibility) first.
    pub fn holders_with_count(&self, cid: &Cid) -> Vec<(NodeId, u32)> {
        self.availability
            .get(cid)
            .map(|holders| {
                holders
                    .iter()
                    .map(|h| (h.node_id, h.piece_count))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record the K value used for RLNC encoding of `cid`.
    ///
    /// Uses first-writer-wins semantics: once a K value is recorded for a CID,
    /// subsequent calls with a different K are silently ignored.
    pub fn record_k(&self, cid: &Cid, k: u32) {
        self.k_values.entry(*cid).or_insert(k);
    }

    /// Return the recorded K value for `cid`, if any.
    pub fn get_k(&self, cid: &Cid) -> Option<u32> {
        self.k_values.get(cid).map(|v| *v)
    }

    /// Remove all piece records for `node_id` across every tracked CID.
    ///
    /// Call this when SWIM declares a node dead or when it gracefully departs.
    /// Also cleans up K values for CIDs that no longer have any holders.
    pub fn remove_node(&self, node_id: &NodeId) {
        let mut removed = 0usize;
        let mut emptied_cids = Vec::new();
        self.availability.retain(|cid, holders| {
            let before = holders.len();
            holders.retain(|h| &h.node_id != node_id);
            removed += before - holders.len();
            let keep = !holders.is_empty();
            if !keep {
                emptied_cids.push(*cid);
            }
            keep
        });
        // Clean up K values for CIDs with no remaining holders (C7).
        for cid in &emptied_cids {
            self.k_values.remove(cid);
        }
        tracing::debug!(
            node = %node_id,
            records_removed = removed,
            k_values_cleaned = emptied_cids.len(),
            "PieceTracker: node removed"
        );
    }

    /// Remove all holder records whose `last_seen` is older than `max_age`.
    ///
    /// Returns the number of stale records pruned.
    pub fn prune_stale(&self, max_age: Duration) -> usize {
        let cutoff = Instant::now() - max_age;
        let mut total_pruned = 0usize;
        self.availability.retain(|_cid, holders| {
            let before = holders.len();
            holders.retain(|h| h.last_seen > cutoff);
            total_pruned += before - holders.len();
            !holders.is_empty()
        });
        if total_pruned > 0 {
            tracing::info!(pruned = total_pruned, "PieceTracker: stale records pruned");
        }
        total_pruned
    }

    /// Return the number of distinct CIDs currently tracked.
    pub fn cid_count(&self) -> usize {
        self.availability.len()
    }

    /// Return all distinct CIDs currently tracked, sorted in ascending order.
    ///
    /// The sorted order is required by [`HealthScanner`]'s deterministic cursor.
    pub fn sorted_cids(&self) -> Vec<Cid> {
        let mut cids: Vec<Cid> = self.availability.iter().map(|e| *e.key()).collect();
        cids.sort_unstable_by_key(|c| *c.as_bytes());
        cids
    }

    /// Return the nodes that hold pieces for `cid`, deduplicated by node ID.
    pub fn holder_nodes(&self, cid: &Cid) -> Vec<NodeId> {
        self.availability
            .get(cid)
            .map(|holders| holders.iter().map(|h| h.node_id).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(node_id: NodeId, piece_count: u32) -> PieceHolder {
        PieceHolder {
            node_id,
            piece_count,
            last_seen: Instant::now(),
        }
    }

    #[test]
    fn starts_empty() {
        let tracker = PieceTracker::new();
        assert_eq!(tracker.cid_count(), 0);
    }

    #[test]
    fn record_and_get() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"content");
        let node = NodeId::generate();

        tracker.record_piece(&cid, holder(node, 1));
        let holders = tracker.get_holders(&cid);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].node_id, node);
    }

    #[test]
    fn available_count_sums_piece_counts() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"pieces");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 2));
        tracker.record_piece(&cid, holder(n2, 1));

        // Sum of piece counts: 2 + 1 = 3.
        assert_eq!(tracker.available_count(&cid), 3);
    }

    #[test]
    fn available_count_dedup_same_node() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"dedup");
        let n1 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 1));
        tracker.record_piece(&cid, holder(n1, 3)); // same node, more pieces

        // piece_count should be max(1, 3) = 3.
        assert_eq!(tracker.available_count(&cid), 3);
        assert_eq!(tracker.local_piece_count(&cid, &n1), 3);
    }

    #[test]
    fn available_count_unknown_cid() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"unknown");
        assert_eq!(tracker.available_count(&cid), 0);
    }

    #[test]
    fn local_piece_count_returns_count() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"local");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 5));
        tracker.record_piece(&cid, holder(n2, 2));

        assert_eq!(tracker.local_piece_count(&cid, &n1), 5);
        assert_eq!(tracker.local_piece_count(&cid, &n2), 2);
        assert_eq!(tracker.local_piece_count(&cid, &NodeId::generate()), 0);
    }

    #[test]
    fn holders_with_count_returns_all() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"counts");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 3));
        tracker.record_piece(&cid, holder(n2, 1));

        let counts = tracker.holders_with_count(&cid);
        assert_eq!(counts.len(), 2);
        assert!(counts.contains(&(n1, 3)));
        assert!(counts.contains(&(n2, 1)));
    }

    #[test]
    fn remove_node_purges_records() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"multi");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 1));
        tracker.record_piece(&cid, holder(n2, 1));
        assert_eq!(tracker.get_holders(&cid).len(), 2);

        tracker.remove_node(&n1);
        let remaining = tracker.get_holders(&cid);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].node_id, n2);
    }

    #[test]
    fn remove_node_cleans_empty_cid() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"lonely");
        let node = NodeId::generate();
        tracker.record_piece(&cid, holder(node, 1));
        assert_eq!(tracker.cid_count(), 1);

        tracker.remove_node(&node);
        assert_eq!(tracker.cid_count(), 0);
    }

    #[test]
    fn prune_stale_removes_old_records() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"stale");
        let node = NodeId::generate();

        // Insert a record with an artificially old last_seen.
        let old_holder = PieceHolder {
            node_id: node,
            piece_count: 1,
            last_seen: Instant::now() - Duration::from_secs(3600),
        };
        tracker.record_piece(&cid, old_holder);
        assert_eq!(tracker.available_count(&cid), 1);

        let pruned = tracker.prune_stale(Duration::from_secs(60));
        assert_eq!(pruned, 1);
        assert_eq!(tracker.available_count(&cid), 0);
    }

    #[test]
    fn sorted_cids_returns_sorted_order() {
        let tracker = PieceTracker::new();
        let node = NodeId::generate();
        let cids: Vec<Cid> = (0u8..5).map(|i| Cid::from_data(&[i])).collect();

        for cid in &cids {
            tracker.record_piece(cid, holder(node, 1));
        }

        let sorted = tracker.sorted_cids();
        assert_eq!(sorted.len(), 5);
        // Verify each adjacent pair is in non-decreasing order.
        for window in sorted.windows(2) {
            assert!(window[0].as_bytes() <= window[1].as_bytes());
        }
    }

    #[test]
    fn record_and_get_k() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"k-test");
        tracker.record_k(&cid, 16);
        assert_eq!(tracker.get_k(&cid), Some(16));
    }

    #[test]
    fn record_k_first_writer_wins() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"k-first-write");
        tracker.record_k(&cid, 8);
        tracker.record_k(&cid, 32);
        assert_eq!(tracker.get_k(&cid), Some(8));
    }

    #[test]
    fn remove_node_cleans_k_values() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"k-cleanup");
        let node = NodeId::generate();
        tracker.record_piece(&cid, holder(node, 1));
        tracker.record_k(&cid, 16);

        // After removing the only holder, K value should be cleaned up.
        tracker.remove_node(&node);
        assert_eq!(tracker.cid_count(), 0);
        assert_eq!(tracker.get_k(&cid), None);
    }

    #[test]
    fn update_last_seen_on_duplicate() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"update");
        let node = NodeId::generate();

        let h1 = PieceHolder {
            node_id: node,
            piece_count: 1,
            last_seen: Instant::now() - Duration::from_secs(120),
        };
        tracker.record_piece(&cid, h1);

        let h2 = PieceHolder {
            node_id: node,
            piece_count: 1,
            last_seen: Instant::now(),
        };
        tracker.record_piece(&cid, h2);

        // Still one holder record, not two.
        assert_eq!(tracker.get_holders(&cid).len(), 1);
    }
}
