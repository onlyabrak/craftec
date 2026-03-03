//! [`HealthScanner`] — continuous 1%-per-cycle CID health scan engine.
//!
//! # Scan strategy
//!
//! The scanner maintains a sorted list of all known CIDs and advances a cursor
//! through it, processing `scan_percent` (default 1%) per cycle.  Over 100 cycles
//! every CID is visited exactly once.
//!
//! At the default `interval / 100` sleep between cycles (e.g., 36 seconds when
//! `interval = 3600s`), a full scan completes in 3600 seconds = 1 hour.
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
//! `target` is computed as `ceil(2.0 + 16.0 / k as f64)` — the Craftec redundancy
//! formula.  For `k = 32`, `target = ceil(2.5) = 3`.
//!
//! # Shutdown
//!
//! The [`HealthScanner::run`] loop listens for a `tokio::sync::broadcast` shutdown
//! signal and exits cleanly when received.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use craftec_obj::ContentAddressedStore;

use crate::error::Result;
use crate::repair::RepairRequest;
use crate::tracker::PieceTracker;

/// Default minimum coded pieces required to reconstruct (matches `K_DEFAULT` from craftec-types).
const DEFAULT_K: u32 = 32;

/// Compute the target piece count for a given `k` using the Craftec redundancy formula:
/// `target = ceil(2.0 + 16.0 / k)`.
fn target_piece_count(k: u32) -> u32 {
    let k = k.max(1) as f64;
    (2.0 + 16.0 / k).ceil() as u32
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
    /// Total interval over which all CIDs should be visited (default 3600 s).
    ///
    /// The cycle sleep time is `interval / 100` (or `interval * scan_percent`).
    interval: Duration,
    /// Live piece availability map.
    piece_tracker: Arc<PieceTracker>,
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
    /// - `interval`: Total time for 100% CID coverage.  Default: 3600 s.
    pub fn new(
        store: Arc<ContentAddressedStore>,
        piece_tracker: Arc<PieceTracker>,
        interval: Duration,
    ) -> Self {
        tracing::info!(
            interval_secs = interval.as_secs(),
            "HealthScanner: initialised"
        );
        Self {
            store,
            scan_percent: 0.01,
            interval,
            piece_tracker,
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
    /// The scanner fetches the current sorted CID list, takes `scan_percent` of
    /// them starting from the saved cursor position, checks each one, and advances
    /// the cursor for the next call.
    pub async fn scan_cycle(&self) -> Result<Vec<RepairRequest>> {
        // Fetch the sorted CID list from the piece tracker (canonical source of known CIDs).
        let all_cids = self.piece_tracker.sorted_cids();

        if all_cids.is_empty() {
            tracing::trace!("HealthScan: no CIDs tracked — skipping cycle");
            return Ok(Vec::new());
        }

        let total = all_cids.len();
        let batch_size = ((total as f64 * self.scan_percent).ceil() as usize).max(1);

        // Advance the cursor, wrapping at the end.
        let start = self.last_scan_index.load(Ordering::Relaxed) % total;
        let end = (start + batch_size).min(total);

        let batch = &all_cids[start..end];

        // Handle wrap-around: if the cursor reached the end, also grab from the beginning.
        let wrapped: &[craftec_types::Cid] = if start + batch_size > total {
            let overflow = (start + batch_size) - total;
            &all_cids[..overflow.min(total)]
        } else {
            &[]
        };

        // Advance the cursor for the next cycle.
        self.last_scan_index
            .store((start + batch_size) % total, Ordering::Relaxed);

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
            "HealthScan: cycle complete"
        );

        Ok(repairs)
    }

    // ── Background run loop ────────────────────────────────────────────────

    /// Run the scanner indefinitely, sleeping `interval / 100` between cycles.
    ///
    /// Emitted [`RepairRequest`]s are forwarded to `repair_tx` for the
    /// [`RepairExecutor`] to process.  Exits cleanly on shutdown signal.
    pub async fn run(
        &self,
        repair_tx: tokio::sync::mpsc::Sender<RepairRequest>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) {
        let cycle_sleep = self.interval.div_f64(100.0);

        tracing::info!(
            cycle_sleep_ms = cycle_sleep.as_millis(),
            "HealthScanner: background loop started"
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(cycle_sleep) => {
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
        let k = DEFAULT_K;
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
        // 2.0 + 16/32 = 2.5 → ceil = 3
        assert_eq!(target_piece_count(32), 3);
    }

    #[test]
    fn target_piece_count_k1() {
        // 2.0 + 16/1 = 18.0 → ceil = 18
        assert_eq!(target_piece_count(1), 18);
    }

    #[test]
    fn target_piece_count_k4() {
        // 2.0 + 16/4 = 6.0 → ceil = 6
        assert_eq!(target_piece_count(4), 6);
    }

    #[test]
    fn target_piece_count_k16() {
        // 2.0 + 16/16 = 3.0 → ceil = 3
        assert_eq!(target_piece_count(16), 3);
    }

    #[test]
    fn target_piece_count_minimum_k() {
        // k=0 is clamped to k=1 internally.
        // No panic on k=0.
        let _ = target_piece_count(0);
    }
}
