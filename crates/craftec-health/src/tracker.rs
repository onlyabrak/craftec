//! [`PieceTracker`] — live piece availability map for health monitoring.
//!
//! The tracker maintains a concurrent map of `Cid → Vec<PieceHolder>` recording
//! which nodes currently hold which coded pieces for each known CID.  This map is
//! the primary data source for [`HealthScanner`] when determining whether a CID
//! has enough redundancy.
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

/// A record indicating that `node_id` holds coded piece `piece_index` for some CID.
#[derive(Debug, Clone)]
pub struct PieceHolder {
    /// The node that holds this piece.
    pub node_id: NodeId,
    /// The coded piece index within the RLNC generation for this CID.
    pub piece_index: u32,
    /// Wall-clock time when this holder record was last confirmed (via ping or announcement).
    pub last_seen: Instant,
}

// ── PieceTracker ────────────────────────────────────────────────────────────

/// Concurrent, lock-free piece availability tracker.
///
/// Cheap to clone — all clones share the same underlying map via `Arc`.
#[derive(Clone, Default)]
pub struct PieceTracker {
    /// `Cid` → list of (node, piece_index) records.
    availability: Arc<DashMap<Cid, Vec<PieceHolder>>>,
}

impl PieceTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        tracing::debug!("PieceTracker: initialised");
        Self::default()
    }

    /// Record that `holder.node_id` holds `holder.piece_index` for `cid`.
    ///
    /// If a record for the same `(node_id, piece_index)` pair already exists,
    /// the `last_seen` timestamp is updated in place.
    pub fn record_piece(&self, cid: &Cid, holder: PieceHolder) {
        tracing::trace!(
            cid = %cid,
            node = %holder.node_id,
            piece = holder.piece_index,
            "PieceTracker: recording piece holder"
        );
        let mut entry = self.availability.entry(*cid).or_default();
        // Update existing record if (node_id, piece_index) matches.
        if let Some(existing) = entry
            .iter_mut()
            .find(|h| h.node_id == holder.node_id && h.piece_index == holder.piece_index)
        {
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

    /// Return the number of distinct coded pieces known to be available for `cid`.
    ///
    /// Counts unique `piece_index` values across all holders.  Multiple nodes
    /// holding the same `piece_index` count as one piece (for threshold arithmetic).
    pub fn available_count(&self, cid: &Cid) -> u32 {
        self.availability
            .get(cid)
            .map(|holders| {
                let unique: std::collections::HashSet<u32> =
                    holders.iter().map(|h| h.piece_index).collect();
                unique.len() as u32
            })
            .unwrap_or(0)
    }

    /// Remove all piece records for `node_id` across every tracked CID.
    ///
    /// Call this when SWIM declares a node dead or when it gracefully departs.
    pub fn remove_node(&self, node_id: &NodeId) {
        let mut removed = 0usize;
        self.availability.retain(|_cid, holders| {
            let before = holders.len();
            holders.retain(|h| &h.node_id != node_id);
            removed += before - holders.len();
            !holders.is_empty() // Drop empty CID entries.
        });
        tracing::debug!(
            node = %node_id,
            records_removed = removed,
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
        let mut seen = std::collections::HashSet::new();
        self.availability
            .get(cid)
            .map(|holders| {
                holders
                    .iter()
                    .filter(|h| seen.insert(h.node_id))
                    .map(|h| h.node_id)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(node_id: NodeId, piece_index: u32) -> PieceHolder {
        PieceHolder {
            node_id,
            piece_index,
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

        tracker.record_piece(&cid, holder(node, 0));
        let holders = tracker.get_holders(&cid);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].node_id, node);
    }

    #[test]
    fn available_count_unique_pieces() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"pieces");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 0));
        tracker.record_piece(&cid, holder(n2, 0)); // same piece, different node
        tracker.record_piece(&cid, holder(n1, 1));

        // Two unique piece indices: 0 and 1.
        assert_eq!(tracker.available_count(&cid), 2);
    }

    #[test]
    fn available_count_unknown_cid() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"unknown");
        assert_eq!(tracker.available_count(&cid), 0);
    }

    #[test]
    fn remove_node_purges_records() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"multi");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        tracker.record_piece(&cid, holder(n1, 0));
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
        tracker.record_piece(&cid, holder(node, 0));
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
            piece_index: 0,
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
            tracker.record_piece(cid, holder(node, 0));
        }

        let sorted = tracker.sorted_cids();
        assert_eq!(sorted.len(), 5);
        // Verify each adjacent pair is in non-decreasing order.
        for window in sorted.windows(2) {
            assert!(window[0].as_bytes() <= window[1].as_bytes());
        }
    }

    #[test]
    fn update_last_seen_on_duplicate() {
        let tracker = PieceTracker::new();
        let cid = Cid::from_data(b"update");
        let node = NodeId::generate();

        let h1 = PieceHolder {
            node_id: node,
            piece_index: 0,
            last_seen: Instant::now() - Duration::from_secs(120),
        };
        tracker.record_piece(&cid, h1);

        let h2 = PieceHolder {
            node_id: node,
            piece_index: 0,
            last_seen: Instant::now(),
        };
        tracker.record_piece(&cid, h2);

        // Still one holder record, not two.
        assert_eq!(tracker.get_holders(&cid).len(), 1);
    }
}
