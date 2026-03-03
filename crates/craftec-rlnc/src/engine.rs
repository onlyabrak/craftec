//! Concurrent RLNC engine with semaphore-controlled parallelism.
//!
//! [`RlncEngine`] is the top-level coordinator that wraps the encode, decode,
//! and recode primitives behind an `async` interface backed by a
//! [`tokio::sync::Semaphore`].  The semaphore limits concurrent RLNC operations
//! to **8**, preventing runaway CPU usage when many tasks request coding at the
//! same time.
//!
//! # Design
//!
//! Each `encode`, `decode`, and `recode` call:
//! 1. Acquires a permit from the semaphore (blocks if 8 are already in flight).
//! 2. Runs the CPU-bound work **synchronously on the async thread**.
//!    (RLNC operations on typical piece sizes complete in microseconds and
//!    do not warrant spawning a blocking thread via `spawn_blocking`.)
//! 3. Releases the permit automatically via RAII when the guard drops.
//!
//! [`RlncMetrics`] tracks cumulative operation counts and bytes processed
//! using `AtomicU64` so metrics can be read from any thread without locking.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::info;

use craftec_types::piece::CodedPiece;

use crate::decoder::RlncDecoder;
use crate::encoder::RlncEncoder;
use crate::error::{Result, RlncError};
use crate::recoder::RlncRecoder;

// ── RlncMetrics ───────────────────────────────────────────────────────────────

/// Cumulative metrics for an [`RlncEngine`] instance.
///
/// All counters are monotonically increasing and safe to read from any thread.
#[derive(Debug, Default)]
pub struct RlncMetrics {
    /// Total number of successful encode operations.
    pub encodes: AtomicU64,
    /// Total number of successful decode operations.
    pub decodes: AtomicU64,
    /// Total number of successful recode operations.
    pub recodes: AtomicU64,
    /// Total bytes encoded (sum of input data sizes).
    pub encode_bytes: AtomicU64,
}

impl RlncMetrics {
    /// Return a snapshot of the current metric values.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            encodes: self.encodes.load(Ordering::Relaxed),
            decodes: self.decodes.load(Ordering::Relaxed),
            recodes: self.recodes.load(Ordering::Relaxed),
            encode_bytes: self.encode_bytes.load(Ordering::Relaxed),
        }
    }
}

/// A non-atomic snapshot of [`RlncMetrics`] at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Total encode operations.
    pub encodes: u64,
    /// Total decode operations.
    pub decodes: u64,
    /// Total recode operations.
    pub recodes: u64,
    /// Total bytes submitted for encoding.
    pub encode_bytes: u64,
}

// ── RlncEngine ────────────────────────────────────────────────────────────────

/// Concurrent RLNC engine — the primary entry point for coding operations.
///
/// Clone cheaply: the internal [`Semaphore`] and metrics are reference-counted.
///
/// # Example
///
/// ```rust,no_run
/// use craftec_rlnc::engine::RlncEngine;
///
/// #[tokio::main]
/// async fn main() {
///     let engine = RlncEngine::new();
///     let data = vec![0u8; 8192];
///     let pieces = engine.encode(&data, 32).await.unwrap();
///     let recovered = engine.decode(32, pieces[0].data.len(), &pieces).await.unwrap();
///     assert_eq!(&recovered[..data.len()], data.as_slice());
/// }
/// ```
#[derive(Clone)]
pub struct RlncEngine {
    /// Limits concurrent RLNC operations to at most 8.
    semaphore: Arc<Semaphore>,
    /// Shared metrics counter.
    metrics: Arc<RlncMetrics>,
}

impl RlncEngine {
    /// Maximum number of RLNC operations that may run concurrently.
    pub const MAX_CONCURRENCY: usize = 8;

    /// Create a new [`RlncEngine`] with a concurrency limit of 8.
    pub fn new() -> Self {
        info!("RLNC engine initialized with concurrency limit {}", Self::MAX_CONCURRENCY);
        Self {
            semaphore: Arc::new(Semaphore::new(Self::MAX_CONCURRENCY)),
            metrics: Arc::new(RlncMetrics::default()),
        }
    }

    /// Return a reference to the shared metrics.
    pub fn metrics(&self) -> &RlncMetrics {
        &self.metrics
    }

    /// Acquire a semaphore permit, returning an error if the semaphore is closed.
    async fn acquire(&self) -> Result<SemaphorePermit<'_>> {
        self.semaphore
            .acquire()
            .await
            .map_err(|_| RlncError::SemaphoreError)
    }

    /// Encode `data` into coded pieces using generation size `k`.
    ///
    /// Produces `target_pieces(k)` coded pieces (one redundant set).
    ///
    /// # Arguments
    ///
    /// * `data` — raw bytes to encode.
    /// * `k`    — generation size.
    ///
    /// # Errors
    ///
    /// Propagates any error from [`RlncEncoder::new`].
    pub async fn encode(&self, data: &[u8], k: u32) -> Result<Vec<CodedPiece>> {
        let _permit = self.acquire().await?;

        let encoder = RlncEncoder::new(data, k)?;
        let n = encoder.target_pieces() as usize;
        let result = encoder.encode_n(n);

        self.metrics.encodes.fetch_add(1, Ordering::Relaxed);
        self.metrics.encode_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);

        info!(
            size = data.len(),
            pieces = result.len(),
            k = k,
            "RLNC: encode complete"
        );

        Ok(result)
    }

    /// Decode `pieces` back into the original data.
    ///
    /// Feeds all pieces into a new [`RlncDecoder`]; returns an error if the
    /// matrix does not reach full rank.
    ///
    /// # Arguments
    ///
    /// * `k`          — generation size.
    /// * `piece_size` — byte size of each coded piece's data payload.
    /// * `pieces`     — slice of coded pieces (must contain ≥ `k` independent ones).
    ///
    /// # Errors
    ///
    /// * [`RlncError::InsufficientPieces`] — not enough independent pieces.
    /// * [`RlncError::DecodeFailed`]       — Gaussian elimination failed.
    pub async fn decode(
        &self,
        k: u32,
        piece_size: usize,
        pieces: &[CodedPiece],
    ) -> Result<Vec<u8>> {
        let _permit = self.acquire().await?;

        let mut decoder = RlncDecoder::new(k, piece_size);
        for piece in pieces {
            match decoder.add_piece(piece) {
                Ok(_) => {}
                Err(RlncError::CodingVectorLengthMismatch { .. } |
                    RlncError::InvalidPieceSize { .. }) => {
                    // Skip malformed pieces rather than aborting.
                    tracing::warn!("skipping malformed piece during decode");
                }
                Err(e) => return Err(e),
            }
            if decoder.is_decodable() {
                break;
            }
        }

        let result = decoder.decode()?;

        self.metrics.decodes.fetch_add(1, Ordering::Relaxed);

        info!(
            k = k,
            pieces_used = pieces.len(),
            bytes = result.len(),
            "RLNC: decode complete"
        );

        Ok(result)
    }

    /// Recode a set of coded pieces into one new coded piece.
    ///
    /// See [`RlncRecoder::recode`] for the full contract.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`RlncRecoder::recode`].
    pub async fn recode(&self, pieces: &[CodedPiece]) -> Result<CodedPiece> {
        let _permit = self.acquire().await?;
        let result = RlncRecoder::recode(pieces)?;
        self.metrics.recodes.fetch_add(1, Ordering::Relaxed);
        Ok(result)
    }
}

impl Default for RlncEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::RlncEncoder;
    use std::sync::Arc;

    // Helper: build a standard test dataset.
    fn test_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn encode_decode_roundtrip() {
        let engine = RlncEngine::new();
        let data = test_data(4096);
        let pieces = engine.encode(&data, 8).await.unwrap();
        let piece_size = pieces[0].data.len();
        let recovered = engine.decode(8, piece_size, &pieces).await.unwrap();
        assert_eq!(&recovered[..data.len()], data.as_slice());
    }

    #[tokio::test]
    async fn encode_decode_k32() {
        let engine = RlncEngine::new();
        let data = test_data(32 * 256);
        let pieces = engine.encode(&data, 32).await.unwrap();
        let piece_size = pieces[0].data.len();
        let recovered = engine.decode(32, piece_size, &pieces).await.unwrap();
        assert_eq!(&recovered[..data.len()], data.as_slice());
    }

    #[tokio::test]
    async fn recode_via_engine() {
        let engine = RlncEngine::new();
        let data = test_data(1024);
        let pieces = engine.encode(&data, 4).await.unwrap();
        let recoded = engine.recode(&pieces[..3]).await.unwrap();
        assert_eq!(recoded.cid, pieces[0].cid);
        assert!(recoded.verify_piece_id());
    }

    #[tokio::test]
    async fn metrics_increment() {
        let engine = RlncEngine::new();
        let data = test_data(512);

        let pieces = engine.encode(&data, 4).await.unwrap();
        let snap = engine.metrics().snapshot();
        assert_eq!(snap.encodes, 1);
        assert_eq!(snap.encode_bytes, 512);

        let piece_size = pieces[0].data.len();
        engine.decode(4, piece_size, &pieces).await.unwrap();
        let snap = engine.metrics().snapshot();
        assert_eq!(snap.decodes, 1);

        engine.recode(&pieces[..2]).await.unwrap();
        let snap = engine.metrics().snapshot();
        assert_eq!(snap.recodes, 1);
    }

    #[tokio::test]
    async fn semaphore_limits_concurrency() {
        // Launch 16 concurrent encodes and verify all complete without error.
        // (The semaphore allows 8 at a time; the rest queue up.)
        let engine = Arc::new(RlncEngine::new());
        let data = Arc::new(test_data(2048));

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let engine = Arc::clone(&engine);
                let data = Arc::clone(&data);
                tokio::spawn(async move {
                    engine.encode(&data, 8).await
                })
            })
            .collect();

        for handle in handles {
            let result = handle.await.expect("task panicked");
            assert!(result.is_ok(), "encode failed: {:?}", result);
        }

        let snap = engine.metrics().snapshot();
        assert_eq!(snap.encodes, 16, "expected 16 encodes, got {}", snap.encodes);
    }

    #[tokio::test]
    async fn engine_is_clone_safe() {
        let engine1 = RlncEngine::new();
        let engine2 = engine1.clone();

        let data = test_data(256);
        let _pieces = engine1.encode(&data, 4).await.unwrap();
        // Clone should see the same metrics counter.
        let snap = engine2.metrics().snapshot();
        assert_eq!(snap.encodes, 1);
    }

    #[tokio::test]
    async fn decode_insufficient_pieces_error() {
        let engine = RlncEngine::new();
        // Only 2 pieces, need 8.
        let encoder = RlncEncoder::new(&test_data(512), 8).unwrap();
        let pieces = encoder.encode_n(2);
        let piece_size = pieces[0].data.len();
        let result = engine.decode(8, piece_size, &pieces).await;
        assert!(matches!(result, Err(RlncError::InsufficientPieces { .. })));
    }
}
