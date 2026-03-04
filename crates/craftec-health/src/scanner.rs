//! [`HealthScanner`] — continuous 1%-per-cycle CID health scan engine.
//!
//! # Scan strategy
//!
//! The scanner maintains a sorted list of all known CIDs and advances a cursor
//! through it, processing `scan_percent` (default 1%) per cycle.  Over 100 cycles
//! every CID is visited exactly once.
//!
//! At the default cycle interval of 300 s (5 minutes), a full scan completes
//! in 100 × 300 s = 30 000 seconds ≈ 8.3 hours.
//!
//! # Scan eligibility
//!
//! A node only evaluates CIDs for which it holds ≥2 coded pieces locally.
//! This ensures the scanner only flags CIDs it can actually repair (RLNC
//! recoding requires ≥2 pieces).
//!
//! # Priority
//!
//! Within each batch the scanner checks `available_pieces` against two thresholds:
//!
//! | Condition | Severity |
//! |---|---|
//! | `available < k` | [`RepairRequest::Critical`] — data loss imminent |
//! | `available < target` | [`RepairRequest::Normal`] — redundancy degraded |
//!
//! `target` is the total coded piece count: `n = k × ceil(2.0 + 16/k)`.
//! For `k = 32`, `target = 96`.  For `k = 8`, `target = 32`.
//!
//! # Shutdown
//!
//! The [`HealthScanner::run`] loop listens for a `tokio::sync::broadcast` shutdown
//! signal and exits cleanly when received.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use craftec_obj::ContentAddressedStore;
use craftec_types::NodeId;

use crate::error::Result;
use crate::repair::RepairRequest;
use crate::tracker::PieceTracker;

/// Default minimum coded pieces required to reconstruct (matches `K_DEFAULT` from craftec-types).
const DEFAULT_K: u32 = 32;

/// Compute the target piece count for a given `k` using the Craftec redundancy formula:
/// `target = k × ceil(2.0 + 16.0 / k)`.
///
/// Examples:
/// - k=32 → 32 × ceil(2.5) = 32 × 3 = 96
/// - k=8  → 8 × ceil(4.0) = 8 × 4 = 32
/// - k=16 → 16 × ceil(3.0) = 16 × 3 = 48
/// - k=1  → 1 × ceil(18.0) = 1 × 18 = 18
/// - k=4  → 4 × ceil(6.0) = 4 × 6 = 24
fn target_piece_count(k: u32) -> u32 {
    let k = k.max(1);
    let factor = (2.0 + 16.0 / k as f64).ceil() as u32;
    k * factor
}

// ── HealthScanner ─────────────────────────────────────────────────────────────

/// Scans 1% of all known CIDs per cycle and emits [`RepairRequest`]s for any
/// CID whose piece availability has fallen below the redundancy target.
pub struct HealthScanner {
    /// The underlying content-addressed store — source of the CID list.
    #[allow(dead_code)]
    store: Arc<ContentAddressedStore>,
    /// Fraction of all CIDs to scan per cycle (default `0.01` = 1%).
    scan_percent: f64,
    /// Duration between each scan cycle (default 300 s = 5 minutes).
    /// Full coverage = 100 cycles × this value.
    cycle_interval: Duration,
    /// Live piece availability map.
    piece_tracker: Arc<PieceTracker>,
    /// This node's identity — used for scan eligibility (≥2 local pieces).
    node_id: NodeId,
    /// Cursor into the sorted CID list — advances each cycle, wraps at the end.
    last_scan_index: AtomicUsize,
}

impl HealthScanner {
    /// Create a new `HealthScanner`.
    ///
    /// # Parameters
    ///
    /// - `store`: The content-addressed store that owns the canonical CID list.
    /// - `piece_tracker`: Shared availability tracker updated by the net layer.
    /// - `cycle_interval`: Sleep between each 1%-scan cycle.  Default: 300 s (5 min).
    /// - `node_id`: This node's identity for scan eligibility filtering.
    pub fn new(
        store: Arc<ContentAddressedStore>,
        piece_tracker: Arc<PieceTracker>,
        cycle_interval: Duration,
        node_id: NodeId,
    ) -> Self {
        tracing::info!(
            cycle_secs = cycle_interval.as_secs(),
            "HealthScanner: initialised"
        );
        Self {
            store,
            scan_percent: 0.01,
            cycle_interval,
            piece_tracker,
            node_id,
            last_scan_index: AtomicUsize::new(0),
        }
    }

    /// Override the scan fraction (default 0.01 = 1%).
    ///
    /// Values outside `(0.0, 1.0]` are clamped to that range.
    pub fn with_scan_percent(mut self, percent: f64) -> Self {
        self.scan_percent = percent.clamp(f64::EPSILON, 1.0);
        self
    }

    // ── Scan cycle ─────────────────────────────────────────────────────────

    /// Run a single scan cycle and return the list of [`RepairRequest`]s found.
    ///
    /// The scanner fetches the current sorted CID list, filters to CIDs where
    /// this node holds ≥2 coded pieces (scan eligibility), takes `scan_percent`
    /// of them starting from the saved cursor position, checks each one, and
    /// advances the cursor for the next call.
    pub async fn scan_cycle(&self) -> Result<Vec<RepairRequest>> {
        let cycle_start = std::time::Instant::now();
        // Fetch the sorted CID list from the piece tracker (canonical source of known CIDs).
        let all_cids = self.piece_tracker.sorted_cids();

        if all_cids.is_empty() {
            tracing::trace!("HealthScan: no CIDs tracked — skipping cycle");
            return Ok(Vec::new());
        }

        // Filter to CIDs where this node holds ≥2 pieces (scan eligibility).
        let eligible_cids: Vec<_> = all_cids
            .into_iter()
            .filter(|cid| self.piece_tracker.local_piece_count(cid, &self.node_id) >= 2)
            .collect();

        if eligible_cids.is_empty() {
            tracing::trace!("HealthScan: no eligible CIDs (need ≥2 local pieces) — skipping cycle");
            return Ok(Vec::new());
        }

        let total = eligible_cids.len();
        let batch_size = ((total as f64 * self.scan_percent).ceil() as usize).max(1);

        // Advance the cursor, wrapping at the end (T14: use Acquire ordering).
        let start = self.last_scan_index.load(Ordering::Acquire) % total;
        let end = (start + batch_size).min(total);

        let batch = &eligible_cids[start..end];

        // Handle wrap-around: if the cursor reached the end, also grab from the beginning.
        let wrapped: &[craftec_types::Cid] = if start + batch_size > total {
            let overflow = (start + batch_size) - total;
            &eligible_cids[..overflow.min(total)]
        } else {
            &[]
        };

        // Advance the cursor for the next cycle (T14: use Release ordering).
        self.last_scan_index
            .store((start + batch_size) % total, Ordering::Release);

        tracing::trace!(
            cursor = start,
            batch_size,
            total_cids = total,
            "HealthScan: cycle start"
        );

        // Evaluate each CID in the batch.
        let mut repairs = Vec::new();

        for cid in batch.iter().chain(wrapped.iter()) {
            if let Some(req) = self.evaluate_cid(cid) {
                repairs.push(req);
            }
        }

        tracing::info!(
            scanned = batch.len() + wrapped.len(),
            repairs = repairs.len(),
            cursor = start,
            duration_ms = cycle_start.elapsed().as_millis() as u64,
            "HealthScan: cycle complete"
        );

        Ok(repairs)
    }

    // ── Background run loop ────────────────────────────────────────────────

    /// Run the scanner indefinitely, sleeping `cycle_interval` between cycles.
    ///
    /// Emitted [`RepairRequest`]s are forwarded to `repair_tx` for the
    /// [`RepairExecutor`] to process.  Exits cleanly on shutdown signal.
    pub async fn run(
        &self,
        repair_tx: tokio::sync::mpsc::Sender<RepairRequest>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) {
        tracing::info!(
            cycle_interval_ms = self.cycle_interval.as_millis(),
            "HealthScanner: background loop started"
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.cycle_interval) => {
                    match self.scan_cycle().await {
                        Ok(repairs) => {
                            for req in repairs {
                                tracing::warn!(
                                    cid = %req.cid(),
                                    severity = req.severity(),
                                    "HealthScan: repair needed"
                                );
                                if repair_tx.send(req).await.is_err() {
                                    tracing::warn!("HealthScanner: repair channel closed");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "HealthScan: cycle error");
                        }
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("HealthScanner: shutdown signal received — stopping");
                    break;
                }
            }
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Evaluate a single CID and return a [`RepairRequest`] if repair is needed.
    fn evaluate_cid(&self, cid: &craftec_types::Cid) -> Option<RepairRequest> {
        let available = self.piece_tracker.available_count(cid);
        let k = self.piece_tracker.get_k(cid).unwrap_or(DEFAULT_K);
        let target = target_piece_count(k);

        if available < k {
            tracing::warn!(
                cid = %cid,
                available,
                k,
                "HealthScan: critical — below minimum pieces"
            );
            Some(RepairRequest::Critical {
                cid: *cid,
                available,
                k,
            })
        } else if available < target {
            tracing::debug!(
                cid = %cid,
                available,
                target,
                "HealthScan: normal — below target redundancy"
            );
            Some(RepairRequest::Normal {
                cid: *cid,
                available,
                target,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_piece_count_k32() {
        // k=32: 32 × ceil(2.0 + 16/32) = 32 × ceil(2.5) = 32 × 3 = 96
        assert_eq!(target_piece_count(32), 96);
    }

    #[test]
    fn target_piece_count_k1() {
        // k=1: 1 × ceil(2.0 + 16/1) = 1 × ceil(18.0) = 1 × 18 = 18
        assert_eq!(target_piece_count(1), 18);
    }

    #[test]
    fn target_piece_count_k4() {
        // k=4: 4 × ceil(2.0 + 16/4) = 4 × ceil(6.0) = 4 × 6 = 24
        assert_eq!(target_piece_count(4), 24);
    }

    #[test]
    fn target_piece_count_k16() {
        // k=16: 16 × ceil(2.0 + 16/16) = 16 × ceil(3.0) = 16 × 3 = 48
        assert_eq!(target_piece_count(16), 48);
    }

    #[test]
    fn target_piece_count_k8() {
        // k=8: 8 × ceil(2.0 + 16/8) = 8 × ceil(4.0) = 8 × 4 = 32
        assert_eq!(target_piece_count(8), 32);
    }

    #[test]
    fn target_piece_count_minimum_k() {
        // k=0 is clamped to k=1 internally.
        // No panic on k=0.
        let _ = target_piece_count(0);
    }
}
