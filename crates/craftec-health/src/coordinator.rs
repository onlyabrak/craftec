//! [`NaturalSelectionCoordinator`] — deterministic coordinator election for repairs.
//!
//! When a piece shortage is detected, one node must be elected to orchestrate
//! the repair: fetch coded pieces, recode, and distribute.  The Natural Selection
//! algorithm avoids randomness and instead uses a deterministic total order over
//! node quality metrics, making the election verifiable by any participant.
//!
//! # Ranking criteria (highest to lowest priority)
//!
//! 1. **Uptime** (seconds) — longest-running nodes are most reliable.
//! 2. **Reputation score** — quality score from the attestation layer (0.0 – 1.0).
//! 3. **NodeId** (ascending byte order) — a deterministic tiebreaker ensuring
//!    exactly one winner even with identical uptime and reputation.
//!
//! # Usage
//!
//! ```rust,ignore
//! let rankings: Vec<NodeRanking> = providers
//!     .iter()
//!     .map(|n| NodeRanking { node_id: n.id, uptime_secs: n.uptime, reputation_score: n.rep })
//!     .collect();
//!
//! if let Some(coordinator) = NaturalSelectionCoordinator::select_coordinator(&rankings) {
//!     repair_executor.execute_repair_as_coordinator(coordinator, &request).await?;
//! }
//! ```

use craftec_types::NodeId;

// ── NodeRanking ──────────────────────────────────────────────────────────────

/// Quality metrics for a candidate coordinator node.
///
/// Passed to [`NaturalSelectionCoordinator::select_coordinator`] to rank
/// nodes and elect the best repair coordinator.
#[derive(Debug, Clone)]
pub struct NodeRanking {
    /// The node being ranked.
    pub node_id: NodeId,
    /// Continuous uptime in seconds.  Higher is better.
    pub uptime_secs: u64,
    /// Reputation score in the range `[0.0, 1.0]`.  Higher is better.
    ///
    /// Derived from historical audit results, successful repairs, and
    /// peer reputation reports.
    pub reputation_score: f64,
}

// ── Coordinator ──────────────────────────────────────────────────────────────

/// Stateless coordinator election algorithm.
///
/// All methods are pure functions — the struct has no state.
pub struct NaturalSelectionCoordinator;

impl NaturalSelectionCoordinator {
    /// Elect the best coordinator from `providers`.
    ///
    /// Selection order:
    /// 1. Highest `uptime_secs` wins.
    /// 2. Tie-break on `reputation_score` (highest wins).
    /// 3. Tie-break on `node_id` bytes (ascending — smallest bytes wins).
    ///
    /// Returns `None` if `providers` is empty.
    pub fn select_coordinator(providers: &[NodeRanking]) -> Option<NodeId> {
        let winner = providers.iter().min_by(|a, b| {
            // Sort ascending to get the minimum (= best after inversion).
            // We want: uptime DESC, reputation DESC, node_id ASC.
            b.uptime_secs
                .cmp(&a.uptime_secs) // higher uptime first
                .then_with(|| {
                    b.reputation_score
                        .partial_cmp(&a.reputation_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }) // higher reputation first
                .then_with(|| {
                    a.node_id
                        .as_bytes()
                        .cmp(b.node_id.as_bytes()) // lower NodeId first (tiebreaker)
                })
        })?;

        tracing::debug!(
            selected = %winner.node_id,
            uptime = winner.uptime_secs,
            reputation = winner.reputation_score,
            candidates = providers.len(),
            "NatSel: coordinator selected"
        );

        Some(winner.node_id)
    }

    /// Return the full ranked list of `providers` from best to worst.
    ///
    /// Uses the same total order as [`NaturalSelectionCoordinator::select_coordinator`].
    pub fn rank_providers(providers: &[NodeRanking]) -> Vec<NodeId> {
        let mut sorted = providers.to_vec();
        sorted.sort_by(|a, b| {
            b.uptime_secs
                .cmp(&a.uptime_secs)
                .then_with(|| {
                    b.reputation_score
                        .partial_cmp(&a.reputation_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.node_id.as_bytes().cmp(b.node_id.as_bytes()))
        });

        let ranked: Vec<NodeId> = sorted.iter().map(|r| r.node_id).collect();
        tracing::trace!(
            count = ranked.len(),
            "NatSel: full provider ranking computed"
        );
        ranked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranking(uptime: u64, reputation: f64) -> NodeRanking {
        NodeRanking {
            node_id: NodeId::generate(),
            uptime_secs: uptime,
            reputation_score: reputation,
        }
    }

    fn ranking_with_id(node_id: NodeId, uptime: u64, reputation: f64) -> NodeRanking {
        NodeRanking {
            node_id,
            uptime_secs: uptime,
            reputation_score: reputation,
        }
    }

    #[test]
    fn empty_providers_returns_none() {
        assert!(NaturalSelectionCoordinator::select_coordinator(&[]).is_none());
    }

    #[test]
    fn single_provider_wins() {
        let r = ranking(1000, 0.9);
        let winner = NaturalSelectionCoordinator::select_coordinator(&[r.clone()]);
        assert_eq!(winner, Some(r.node_id));
    }

    #[test]
    fn highest_uptime_wins() {
        let low = ranking(100, 1.0);
        let high = ranking(9999, 0.5);
        let winner = NaturalSelectionCoordinator::select_coordinator(&[low.clone(), high.clone()]);
        assert_eq!(winner, Some(high.node_id));
    }

    #[test]
    fn reputation_breaks_uptime_tie() {
        let same_uptime = 500;
        let low_rep = ranking(same_uptime, 0.4);
        let high_rep = ranking(same_uptime, 0.9);
        let winner =
            NaturalSelectionCoordinator::select_coordinator(&[low_rep, high_rep.clone()]);
        assert_eq!(winner, Some(high_rep.node_id));
    }

    #[test]
    fn node_id_breaks_all_ties() {
        let same_uptime = 500;
        let same_rep = 0.7;
        // Create two nodes with known byte values so we can predict the winner.
        let bytes_a = [0u8; 32]; // all zeros → "smaller"
        let bytes_b = [255u8; 32];
        let id_a = NodeId::from_bytes(bytes_a);
        let id_b = NodeId::from_bytes(bytes_b);

        let ra = ranking_with_id(id_a, same_uptime, same_rep);
        let rb = ranking_with_id(id_b, same_uptime, same_rep);
        let winner = NaturalSelectionCoordinator::select_coordinator(&[rb, ra]);
        // id_a has lower bytes → it should win the tiebreaker.
        assert_eq!(winner, Some(id_a));
    }

    #[test]
    fn rank_providers_order() {
        let r1 = ranking(9999, 0.9); // best
        let r2 = ranking(5000, 0.8); // second
        let r3 = ranking(100, 0.1);  // worst
        let ranked =
            NaturalSelectionCoordinator::rank_providers(&[r3.clone(), r1.clone(), r2.clone()]);
        assert_eq!(ranked[0], r1.node_id);
        assert_eq!(ranked[1], r2.node_id);
        assert_eq!(ranked[2], r3.node_id);
    }

    #[test]
    fn rank_providers_empty() {
        let ranked = NaturalSelectionCoordinator::rank_providers(&[]);
        assert!(ranked.is_empty());
    }
}
