//! [`HybridClock`] — 64-bit Hybrid Logical Clock (spec §42).
//!
//! Layout: `[48-bit millisecond wall clock | 16-bit logical counter]`
//!
//! Properties:
//! - Monotonically increasing within a single node.
//! - Advances on `observe(remote_ts)` to track causal ordering across nodes.
//! - Rejects remote timestamps with >500ms skew (`HlcError::ClockSkew`).
//! - Detects replay attacks via ±30s window (`HlcError::ReplayDetected`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum allowed clock skew between nodes (500ms).
const MAX_SKEW_MS: u64 = 500;

/// Replay detection window (±30 seconds).
const REPLAY_WINDOW_MS: u64 = 30_000;

/// Pack a wall-clock ms and logical counter into a 64-bit HLC timestamp.
fn pack(wall_ms: u64, logical: u16) -> u64 {
    (wall_ms << 16) | (logical as u64)
}

/// Unpack a 64-bit HLC timestamp into (wall_ms, logical).
fn unpack(ts: u64) -> (u64, u16) {
    let wall_ms = ts >> 16;
    let logical = (ts & 0xFFFF) as u16;
    (wall_ms, logical)
}

/// Get the current wall-clock time in milliseconds since UNIX epoch.
fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Errors from HLC operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlcError {
    /// The remote timestamp is more than [`MAX_SKEW_MS`] ahead of local wall clock.
    ClockSkew {
        local_ms: u64,
        remote_ms: u64,
        skew_ms: u64,
    },
    /// The remote timestamp is outside the ±30s replay window.
    ReplayDetected {
        local_ms: u64,
        remote_ms: u64,
        delta_ms: u64,
    },
}

impl std::fmt::Display for HlcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HlcError::ClockSkew {
                local_ms,
                remote_ms,
                skew_ms,
            } => write!(
                f,
                "HLC clock skew: remote {remote_ms}ms vs local {local_ms}ms (skew {skew_ms}ms > {MAX_SKEW_MS}ms)"
            ),
            HlcError::ReplayDetected {
                local_ms,
                remote_ms,
                delta_ms,
            } => write!(
                f,
                "HLC replay detected: remote {remote_ms}ms vs local {local_ms}ms (delta {delta_ms}ms > {REPLAY_WINDOW_MS}ms)"
            ),
        }
    }
}

impl std::error::Error for HlcError {}

/// A 64-bit Hybrid Logical Clock.
///
/// Thread-safe — all operations use atomic CAS.
pub struct HybridClock {
    /// Packed `[48-bit wall_ms | 16-bit logical]`.
    state: AtomicU64,
}

impl HybridClock {
    /// Create a new HLC seeded from the current wall clock.
    pub fn new() -> Self {
        let now = wall_ms();
        Self {
            state: AtomicU64::new(pack(now, 0)),
        }
    }

    /// Generate a new timestamp, strictly greater than the last one.
    pub fn now(&self) -> u64 {
        loop {
            let old = self.state.load(Ordering::Acquire);
            let (old_wall, old_logical) = unpack(old);
            let current_wall = wall_ms();

            let new_ts = if current_wall > old_wall {
                // Wall clock advanced — reset logical counter.
                pack(current_wall, 0)
            } else {
                // Wall clock hasn't advanced — increment logical.
                if old_logical == u16::MAX {
                    // Logical overflow — force wall clock forward.
                    pack(old_wall + 1, 0)
                } else {
                    pack(old_wall, old_logical + 1)
                }
            };

            if self
                .state
                .compare_exchange_weak(old, new_ts, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return new_ts;
            }
        }
    }

    /// Observe a remote timestamp and advance the local clock.
    ///
    /// Returns `Ok(())` if the remote timestamp is accepted.
    /// Returns `Err(HlcError::ClockSkew)` if the remote is >500ms ahead.
    /// Returns `Err(HlcError::ReplayDetected)` if the remote is >30s old.
    pub fn observe(&self, remote_ts: u64) -> Result<(), HlcError> {
        let (remote_wall, remote_logical) = unpack(remote_ts);
        let current_wall = wall_ms();

        // Check clock skew: remote must not be >500ms ahead of our wall clock.
        if remote_wall > current_wall + MAX_SKEW_MS {
            return Err(HlcError::ClockSkew {
                local_ms: current_wall,
                remote_ms: remote_wall,
                skew_ms: remote_wall - current_wall,
            });
        }

        // Check replay: remote must be within ±30s of our wall clock.
        let delta = remote_wall.abs_diff(current_wall);
        if delta > REPLAY_WINDOW_MS {
            return Err(HlcError::ReplayDetected {
                local_ms: current_wall,
                remote_ms: remote_wall,
                delta_ms: delta,
            });
        }

        // Advance local clock: max(local, remote, wall) with logical increment.
        loop {
            let old = self.state.load(Ordering::Acquire);
            let (old_wall, old_logical) = unpack(old);

            let new_ts = if current_wall > old_wall && current_wall > remote_wall {
                // Wall clock leads — reset logical.
                pack(current_wall, 0)
            } else if old_wall > remote_wall {
                // Local leads — increment logical.
                pack(old_wall, old_logical.saturating_add(1))
            } else if remote_wall > old_wall {
                // Remote leads — adopt remote wall, increment remote logical.
                pack(remote_wall, remote_logical.saturating_add(1))
            } else {
                // Same wall — take max logical + 1.
                let max_logical = old_logical.max(remote_logical);
                if max_logical == u16::MAX {
                    pack(old_wall + 1, 0)
                } else {
                    pack(old_wall, max_logical + 1)
                }
            };

            if self
                .state
                .compare_exchange_weak(old, new_ts, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Check if a timestamp is within the replay window relative to current wall clock.
    pub fn is_within_replay_window(&self, ts: u64) -> bool {
        let (ts_wall, _) = unpack(ts);
        let current = wall_ms();
        let delta = ts_wall.abs_diff(current);
        delta <= REPLAY_WINDOW_MS
    }

    /// Return the current state (for diagnostics).
    pub fn current(&self) -> u64 {
        self.state.load(Ordering::Acquire)
    }
}

impl Default for HybridClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Unpack a packed HLC timestamp for inspection.
pub fn hlc_unpack(ts: u64) -> (u64, u16) {
    unpack(ts)
}

/// Pack wall_ms + logical into an HLC timestamp.
pub fn hlc_pack(wall_ms: u64, logical: u16) -> u64 {
    pack(wall_ms, logical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hlc_monotonic() {
        let clock = HybridClock::new();
        let mut prev = 0u64;
        for _ in 0..1000 {
            let ts = clock.now();
            assert!(ts > prev, "HLC must be strictly monotonic: {ts} <= {prev}");
            prev = ts;
        }
    }

    #[test]
    fn hlc_observe_advances() {
        let clock = HybridClock::new();
        let current = clock.now();
        // Create a timestamp 100ms in the "future" (still within skew tolerance).
        let (wall, _) = unpack(current);
        let future_ts = pack(wall + 100, 0);
        assert!(clock.observe(future_ts).is_ok());
        let after = clock.now();
        assert!(
            after > future_ts,
            "clock should have advanced past the observed timestamp"
        );
    }

    #[test]
    fn hlc_clock_skew_rejected() {
        let clock = HybridClock::new();
        // Create a timestamp 1000ms in the future — exceeds 500ms max skew.
        let far_future = pack(wall_ms() + 1000, 0);
        let result = clock.observe(far_future);
        assert!(matches!(result, Err(HlcError::ClockSkew { .. })));
    }

    #[test]
    fn hlc_replay_detected() {
        let clock = HybridClock::new();
        // Create a timestamp 60 seconds in the past — exceeds ±30s window.
        let old_ts = pack(wall_ms().saturating_sub(60_000), 0);
        let result = clock.observe(old_ts);
        assert!(matches!(result, Err(HlcError::ReplayDetected { .. })));
    }

    #[test]
    fn hlc_pack_unpack_roundtrip() {
        let wall = 1709500000000u64; // some epoch ms
        let logical = 42u16;
        let packed = pack(wall, logical);
        let (w, l) = unpack(packed);
        assert_eq!(w, wall);
        assert_eq!(l, logical);
    }

    #[test]
    fn hlc_zero_timestamp() {
        let (wall, logical) = unpack(0);
        assert_eq!(wall, 0);
        assert_eq!(logical, 0);
    }

    #[test]
    fn hlc_is_within_replay_window() {
        let clock = HybridClock::new();
        let now_ts = clock.now();
        assert!(clock.is_within_replay_window(now_ts));

        // 60 seconds ago — outside window.
        let old = pack(wall_ms().saturating_sub(60_000), 0);
        assert!(!clock.is_within_replay_window(old));
    }
}
