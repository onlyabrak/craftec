//! [`RepairExecutor`] — RLNC recode-and-distribute repair.
//!
//! When [`HealthScanner`] detects that a CID has fewer coded pieces than the
//! target redundancy (n = k × ceil(2 + 16/k)), the `RepairExecutor` orchestrates repair:
//!
//! 1. **Election** — compute deficit, rank holders with ≥2 pieces by quality,
//!    check if this node is in the top-N (where N = deficit). Multiple nodes
//!    repair in parallel — each elected node produces 1 new piece.
//! 2. **Recode** — combine ≥2 locally-held coded pieces into a new coded piece
//!    using [`RlncEngine`]. **This is not decoding.** Recoding is a linear
//!    combination over GF(2⁸); the original data is never reconstructed.
//!    **No network fetch needed** — the repair node already holds ≥2 pieces.
//! 3. **Distribute** — send the new coded piece to a peer, prioritising peers
//!    with exactly 1 piece (so they reach ≥2 and can join future repair),
//!    then peers with 0 pieces.
//!
//! # Why recode, not decode?
//!
//! RLNC recoding allows any node with ≥2 coded pieces to generate a fresh coded
//! piece without the original data.  This means repair can happen entirely among
//! peers that each hold only a fraction of the content, with no single node ever
//! needing to hold the complete data.  This property is impossible with Reed-Solomon
//! erasure codes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use craftec_net::CraftecEndpoint;
use craftec_obj::ContentAddressedStore;
use craftec_rlnc::RlncEngine;
use craftec_types::piece::CodedPiece;
use craftec_types::{Cid, NodeId, WireMessage};

use crate::coordinator::{NaturalSelectionCoordinator, NodeRanking};
use crate::error::{HealthError, Result};
use crate::tracker::PieceTracker;

// ── PieceCidLookup trait ────────────────────────────────────────────────────

/// Trait for looking up coded-piece CIDs for a content CID.
///
/// The repair executor uses this to find which piece CIDs are stored locally
/// for a given content CID, without depending on `craftec-node` directly.
pub trait PieceCidLookup: Send + Sync {
    /// Return the coded-piece CIDs for `content_cid`, if any are known.
    fn piece_cids(&self, content_cid: &Cid) -> Option<Vec<Cid>>;
}

// ── RepairRequest ────────────────────────────────────────────────────────────

/// A request to repair a CID's piece availability, emitted by [`HealthScanner`].
#[derive(Debug, Clone)]
pub enum RepairRequest {
    /// The number of available pieces has dropped below `k` — data may be
    /// permanently lost if repair is not completed urgently.
    ///
    /// `available` < `k` → critical data loss risk.
    Critical {
        /// The content identifier of the under-replicated object.
        cid: Cid,
        /// Number of pieces currently accessible.
        available: u32,
        /// Minimum pieces required to reconstruct (`k`).
        k: u32,
    },

    /// Piece count is above `k` but below `target` — redundancy is degraded.
    Normal {
        /// The content identifier of the under-replicated object.
        cid: Cid,
        /// Number of pieces currently accessible.
        available: u32,
        /// Target piece count from the redundancy formula `k × ceil(2.0 + 16/k)`.
        target: u32,
    },
}

impl RepairRequest {
    /// Return the CID of the object that needs repair.
    pub fn cid(&self) -> &Cid {
        match self {
            RepairRequest::Critical { cid, .. } => cid,
            RepairRequest::Normal { cid, .. } => cid,
        }
    }

    /// Return a human-readable severity label.
    pub fn severity(&self) -> &'static str {
        match self {
            RepairRequest::Critical { .. } => "critical",
            RepairRequest::Normal { .. } => "normal",
        }
    }

    /// Return the target piece count (for deficit computation).
    fn target_count(&self) -> u32 {
        match self {
            RepairRequest::Critical { k, .. } => *k,
            RepairRequest::Normal { target, .. } => *target,
        }
    }

    /// Return the currently available piece count.
    fn available_count(&self) -> u32 {
        match self {
            RepairRequest::Critical { available, .. } => *available,
            RepairRequest::Normal { available, .. } => *available,
        }
    }
}

// ── RepairExecutor ────────────────────────────────────────────────────────────

/// Executes RLNC recode-and-distribute repairs for under-replicated CIDs.
pub struct RepairExecutor {
    /// RLNC engine for recoding (not decoding!) coded pieces.
    rlnc_engine: Arc<RlncEngine>,
    /// Network endpoint for distributing new pieces to peers.
    net: Arc<CraftecEndpoint>,
    /// Piece availability tracker — provides holders and piece counts.
    tracker: Arc<PieceTracker>,
    /// Local content-addressed store for reading locally-held coded pieces.
    store: Arc<ContentAddressedStore>,
    /// Lookup for coded-piece CIDs given a content CID.
    piece_lookup: Arc<dyn PieceCidLookup>,
    /// This node's identity — used for election check.
    node_id: NodeId,
    /// Monotonic counter for unique request IDs.
    next_request_id: AtomicU64,
}

impl RepairExecutor {
    /// Create a new `RepairExecutor`.
    pub fn new(
        rlnc_engine: Arc<RlncEngine>,
        net: Arc<CraftecEndpoint>,
        tracker: Arc<PieceTracker>,
        store: Arc<ContentAddressedStore>,
        piece_lookup: Arc<dyn PieceCidLookup>,
        node_id: NodeId,
    ) -> Self {
        Self {
            rlnc_engine,
            net,
            tracker,
            store,
            piece_lookup,
            node_id,
            next_request_id: AtomicU64::new(0),
        }
    }

    /// Execute a repair for the given `request`.
    ///
    /// # Steps
    ///
    /// 1. Compute deficit = target - available.
    /// 2. Collect rankings from SWIM (uptime/reputation).
    /// 3. Rank providers via Natural Selection.
    /// 4. Check if this node is in the top-N (where N = deficit). If not, return early.
    /// 5. Load ≥2 local coded pieces from the store (no network fetch).
    /// 6. Recode using `RlncEngine::recode` — producing a fresh coded piece.
    /// 7. Select a distribution target with priority: (1) 1-piece holders, (2) 0-piece holders.
    /// 8. Send the recoded piece to the target.
    ///
    /// # Errors
    ///
    /// - [`HealthError::InsufficientPieces`] if fewer than 2 coded pieces can be loaded locally.
    /// - [`HealthError::RepairFailed`] if recode or distribution fails.
    pub async fn execute_repair(&self, request: &RepairRequest) -> Result<()> {
        let cid = request.cid();

        tracing::info!(
            cid = %cid,
            severity = request.severity(),
            "Repair: starting repair"
        );

        // Step 1: compute deficit.
        let deficit = request.target_count().saturating_sub(request.available_count());
        if deficit == 0 {
            tracing::debug!(cid = %cid, "Repair: no deficit — skipping");
            return Ok(());
        }

        // Step 2: collect rankings from SWIM.
        let holders = self.tracker.get_holders(cid);
        let alive_members = self.net.swim().alive_members();
        let rankings: Vec<NodeRanking> = holders
            .iter()
            .filter(|h| self.tracker.local_piece_count(cid, &h.node_id) >= 2)
            .filter(|h| alive_members.contains(&h.node_id) || h.node_id == self.node_id)
            .map(|h| {
                // Uptime and reputation are placeholders until those layers exist.
                // The NodeId tiebreaker ensures deterministic election.
                NodeRanking {
                    node_id: h.node_id,
                    uptime_secs: 0,
                    reputation_score: 0.5,
                }
            })
            .collect();

        if rankings.is_empty() {
            tracing::debug!(cid = %cid, "Repair: no eligible repairers (need ≥2 local pieces)");
            return Ok(());
        }

        // Step 3: rank providers.
        let ranked = NaturalSelectionCoordinator::rank_providers(&rankings);

        // Step 4: check if this node is in the top-N.
        let elected = &ranked[..ranked.len().min(deficit as usize)];
        if !elected.contains(&self.node_id) {
            tracing::debug!(
                cid = %cid,
                deficit,
                elected_count = elected.len(),
                "Repair: not elected for this CID — skipping"
            );
            return Ok(());
        }

        // Step 5: load ≥2 local coded pieces from store.
        let pieces = self.load_local_pieces(cid).await?;

        // Step 6: recode.
        let recoded =
            self.rlnc_engine
                .recode(&pieces)
                .await
                .map_err(|e| HealthError::RepairFailed {
                    cid: cid.to_string(),
                    reason: format!("RLNC recode failed: {e}"),
                })?;

        // Step 7: find a target peer with distribution priority.
        let target = self.select_distribution_target(cid)?;

        // Step 8: distribute the recoded piece.
        let distribute_msg = WireMessage::PieceResponse {
            pieces: vec![recoded],
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
        };

        self.net
            .send_message(&target, &distribute_msg)
            .await
            .map_err(|e| HealthError::RepairFailed {
                cid: cid.to_string(),
                reason: format!("piece distribution failed: {e}"),
            })?;

        tracing::info!(
            cid = %cid,
            target = %target,
            "Repair: recoded and distributed new piece"
        );

        Ok(())
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Load ≥2 locally-held coded pieces from the content-addressed store.
    ///
    /// Uses the piece CID lookup to find which piece CIDs belong to the
    /// content CID, then reads and deserializes them from the local store.
    async fn load_local_pieces(&self, cid: &Cid) -> Result<Vec<CodedPiece>> {
        let piece_cids = self.piece_lookup.piece_cids(cid).ok_or_else(|| {
            HealthError::InsufficientPieces {
                cid: cid.to_string(),
                k: 2,
                available: 0,
            }
        })?;

        let mut pieces = Vec::new();
        for pcid in &piece_cids {
            if pieces.len() >= 2 {
                break;
            }
            match self.store.get(pcid).await {
                Ok(Some(bytes)) => match postcard::from_bytes::<CodedPiece>(&bytes) {
                    Ok(piece) => pieces.push(piece),
                    Err(e) => {
                        tracing::warn!(
                            cid = %cid,
                            piece_cid = %pcid,
                            error = %e,
                            "Repair: failed to deserialize coded piece — skipping"
                        );
                    }
                },
                Ok(None) => {
                    tracing::debug!(
                        piece_cid = %pcid,
                        "Repair: piece CID not found in local store — skipping"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        piece_cid = %pcid,
                        error = %e,
                        "Repair: store read failed — skipping"
                    );
                }
            }
        }

        if pieces.len() < 2 {
            return Err(HealthError::InsufficientPieces {
                cid: cid.to_string(),
                k: 2,
                available: pieces.len() as u32,
            });
        }

        Ok(pieces)
    }

    /// Select a target node to receive the newly recoded piece.
    ///
    /// Distribution priority:
    /// 1. Peers holding exactly 1 piece (bring to ≥2 for repair eligibility).
    /// 2. Peers holding 0 pieces (increase overall redundancy).
    /// 3. Fallback: any alive peer.
    fn select_distribution_target(&self, cid: &Cid) -> Result<NodeId> {
        use rand::seq::SliceRandom;

        let holder_counts = self.tracker.holders_with_count(cid);
        let holder_ids: std::collections::HashSet<NodeId> =
            holder_counts.iter().map(|(id, _)| *id).collect();

        let alive_peers = self.net.swim().alive_members();

        // Priority 1: peers holding exactly 1 piece (bring to ≥2).
        let one_piece_peers: Vec<NodeId> = holder_counts
            .iter()
            .filter(|(id, count)| *count == 1 && alive_peers.contains(id) && *id != self.node_id)
            .map(|(id, _)| *id)
            .collect();

        if let Some(target) = one_piece_peers.choose(&mut rand::thread_rng()) {
            return Ok(*target);
        }

        // Priority 2: peers holding 0 pieces.
        let zero_piece_peers: Vec<NodeId> = alive_peers
            .iter()
            .copied()
            .filter(|id| !holder_ids.contains(id) && *id != self.node_id)
            .collect();

        if let Some(target) = zero_piece_peers.choose(&mut rand::thread_rng()) {
            return Ok(*target);
        }

        // Fallback: any alive peer (excluding self).
        let fallback: Vec<NodeId> = alive_peers
            .into_iter()
            .filter(|id| *id != self.node_id)
            .collect();

        fallback
            .choose(&mut rand::thread_rng())
            .copied()
            .ok_or_else(|| HealthError::RepairFailed {
                cid: cid.to_string(),
                reason: "no candidate peers available for distribution".into(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_request_cid_accessor() {
        let cid = Cid::from_data(b"test");
        let req = RepairRequest::Critical {
            cid,
            available: 1,
            k: 2,
        };
        assert_eq!(req.cid(), &cid);
        assert_eq!(req.severity(), "critical");
    }

    #[test]
    fn normal_severity_label() {
        let cid = Cid::from_data(b"test");
        let req = RepairRequest::Normal {
            cid,
            available: 3,
            target: 5,
        };
        assert_eq!(req.severity(), "normal");
    }

    #[test]
    fn repair_request_deficit() {
        let req = RepairRequest::Normal {
            cid: Cid::from_data(b"deficit"),
            available: 50,
            target: 96,
        };
        assert_eq!(req.target_count(), 96);
        assert_eq!(req.available_count(), 50);
    }

    #[test]
    fn critical_deficit() {
        let req = RepairRequest::Critical {
            cid: Cid::from_data(b"critical"),
            available: 10,
            k: 32,
        };
        assert_eq!(req.target_count(), 32);
        assert_eq!(req.available_count(), 10);
    }
}
