//! RLNC-specific error types.
//!
//! All fallible operations within `craftec-rlnc` return [`RlncError`] wrapped
//! in a [`Result`] alias. Errors are designed to be informative and actionable:
//! consumers can match on variants to determine the exact failure mode without
//! string parsing.

use thiserror::Error;

/// Errors that can occur during RLNC encode, decode, or recode operations.
#[derive(Debug, Error)]
pub enum RlncError {
    /// The caller supplied fewer coded pieces than the generation size `k`
    /// requires to reconstruct the original data.
    ///
    /// The decoder must collect at least `need` linearly-independent pieces
    /// before calling [`decode`](crate::decoder::RlncDecoder::decode).
    #[error("insufficient pieces: have {have}, need {need}")]
    InsufficientPieces {
        /// Number of linearly-independent pieces currently held.
        have: u32,
        /// Minimum number required for decoding (equal to generation size `k`).
        need: u32,
    },

    /// The supplied coded piece is linearly dependent on pieces already in the
    /// decoding matrix and therefore carries no new information.
    ///
    /// Callers should discard the piece and continue collecting more.
    #[error("coded piece is linearly dependent on existing matrix rows")]
    LinearlyDependent,

    /// Gaussian elimination over GF(2⁸) failed to produce a full-rank system.
    ///
    /// This should not occur when `rank == k` — if it does it indicates a bug
    /// in the encoder or data corruption.
    #[error("decode failed: {0}")]
    DecodeFailed(String),

    /// The size of an incoming piece's data payload does not match the
    /// expected piece size for this generation.
    #[error("invalid piece size: expected {expected} bytes, got {got}")]
    InvalidPieceSize {
        /// Expected piece size in bytes.
        expected: usize,
        /// Actual piece size in bytes.
        got: usize,
    },

    /// The RLNC engine's [`tokio::sync::Semaphore`] was closed while waiting
    /// for a permit, indicating the engine is shutting down.
    #[error("RLNC engine semaphore closed (engine is shutting down)")]
    SemaphoreError,

    /// The coding vector length does not match the generation size.
    #[error("coding vector length {got} does not match generation size {expected}")]
    CodingVectorLengthMismatch {
        /// Expected length (equal to generation size `k`).
        expected: usize,
        /// Actual length received.
        got: usize,
    },

    /// Recoding requires at least 2 input pieces from the same generation.
    #[error("recode requires at least 2 input pieces, got {got}")]
    InsufficientRecodeInputs {
        /// Number of pieces actually provided.
        got: usize,
    },

    /// All input pieces to a recode operation must share the same CID.
    #[error("recode input pieces have mismatched CIDs")]
    MismatchedCids,
}

/// Convenience `Result` alias for RLNC operations.
pub type Result<T> = std::result::Result<T, RlncError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insufficient_pieces_display() {
        let e = RlncError::InsufficientPieces { have: 20, need: 32 };
        assert_eq!(e.to_string(), "insufficient pieces: have 20, need 32");
    }

    #[test]
    fn linearly_dependent_display() {
        let e = RlncError::LinearlyDependent;
        assert!(e.to_string().contains("linearly dependent"));
    }

    #[test]
    fn decode_failed_display() {
        let e = RlncError::DecodeFailed("singular matrix".into());
        assert!(e.to_string().contains("singular matrix"));
    }

    #[test]
    fn invalid_piece_size_display() {
        let e = RlncError::InvalidPieceSize {
            expected: 1024,
            got: 512,
        };
        assert_eq!(
            e.to_string(),
            "invalid piece size: expected 1024 bytes, got 512"
        );
    }

    #[test]
    fn semaphore_error_display() {
        let e = RlncError::SemaphoreError;
        assert!(e.to_string().contains("semaphore"));
    }

    #[test]
    fn coding_vector_mismatch_display() {
        let e = RlncError::CodingVectorLengthMismatch {
            expected: 32,
            got: 16,
        };
        assert!(e.to_string().contains("16"));
    }

    #[test]
    fn insufficient_recode_inputs_display() {
        let e = RlncError::InsufficientRecodeInputs { got: 1 };
        assert!(e.to_string().contains("1"));
    }

    #[test]
    fn mismatched_cids_display() {
        let e = RlncError::MismatchedCids;
        assert!(e.to_string().contains("CID"));
    }
}
