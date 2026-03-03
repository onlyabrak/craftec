//! Error types for the `craftec-health` layer.
//!
//! All fallible operations in this crate return [`HealthError`] or the
//! crate-local [`Result`] alias.

use thiserror::Error;

/// Errors that can arise in the Craftec health scanning and repair layer.
#[derive(Debug, Error)]
pub enum HealthError {
    /// There are fewer coded pieces available than the minimum `k` needed for
    /// any operation (recoding or decoding).
    ///
    /// `available` < `k` indicates critical data loss — active repair is required
    /// and may not succeed if too many peers holding pieces have gone offline.
    #[error("insufficient pieces for CID {cid}: need {k}, have {available}")]
    InsufficientPieces {
        /// The content identifier of the under-replicated object.
        cid: String,
        /// The minimum number of pieces required (`k`).
        k: u32,
        /// The number of pieces currently accessible.
        available: u32,
    },

    /// The repair executor could not fetch enough pieces or distribute the
    /// recoded piece to any peer.
    #[error("repair failed for CID {cid}: {reason}")]
    RepairFailed {
        /// The content identifier of the object that could not be repaired.
        cid: String,
        /// A human-readable description of the failure.
        reason: String,
    },

    /// No coordinator could be elected because no eligible nodes were found in
    /// the provider list.
    #[error("coordinator election failed: {0}")]
    CoordinatorElectionFailed(String),

    /// A scan cycle could not complete due to an internal error.
    #[error("scan failed: {0}")]
    ScanFailed(String),

    /// Propagated network error from `craftec-net`.
    #[error("network error: {0}")]
    NetworkError(String),
}

/// Crate-local result alias backed by [`HealthError`].
pub type Result<T, E = HealthError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insufficient_pieces_display() {
        let err = HealthError::InsufficientPieces {
            cid: "abc".into(),
            k: 32,
            available: 5,
        };
        let s = err.to_string();
        assert!(s.contains("abc"));
        assert!(s.contains("32"));
        assert!(s.contains("5"));
    }

    #[test]
    fn repair_failed_display() {
        let err = HealthError::RepairFailed {
            cid: "deadbeef".into(),
            reason: "no peers online".into(),
        };
        assert!(err.to_string().contains("deadbeef"));
        assert!(err.to_string().contains("no peers online"));
    }

    #[test]
    fn coordinator_election_failed_display() {
        let err = HealthError::CoordinatorElectionFailed("empty provider list".into());
        assert!(err.to_string().contains("empty provider list"));
    }

    #[test]
    fn scan_failed_display() {
        let err = HealthError::ScanFailed("store unavailable".into());
        assert!(err.to_string().contains("store unavailable"));
    }
}
