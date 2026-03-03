//! # craftec-obj errors
//!
//! Defines the [`ObjError`] type returned by all fallible operations in the
//! CraftOBJ content-addressed storage layer.
//!
//! ## Design
//!
//! Errors are kept minimal and actionable:
//! - [`ObjError::IntegrityViolation`] indicates on-disk corruption — the stored
//!   bytes no longer match their declared CID. This is a serious fault that
//!   should be logged prominently and trigger repair.
//! - [`ObjError::NotFound`] indicates a miss after both the bloom filter and
//!   disk were checked.
//! - [`ObjError::IoError`] wraps lower-level I/O failures transparently via
//!   `#[from]`.
//! - [`ObjError::StoreFull`] is returned when the host filesystem has no space.

use thiserror::Error;

/// All errors that CraftOBJ operations can produce.
///
/// # Integrity guarantees
///
/// Every object read from the store is verified against its CID using BLAKE3.
/// If the bytes on disk have been corrupted (bitrot, truncation, or deliberate
/// tampering), the read returns [`ObjError::IntegrityViolation`] rather than
/// silently returning bad data.
#[derive(Debug, Error)]
pub enum ObjError {
    /// The stored object's BLAKE3 hash does not match the requested CID.
    ///
    /// This indicates on-disk corruption or tampering. The object should be
    /// deleted and re-fetched from the network.
    #[error("integrity violation for CID {cid}: {msg}")]
    IntegrityViolation {
        /// Hex-encoded CID that was requested.
        cid: String,
        /// Human-readable description of the mismatch.
        msg: String,
    },

    /// The requested CID is not present in this store.
    ///
    /// Both the bloom filter and the filesystem were checked; the object is
    /// definitively absent.
    #[error("object not found: {cid}")]
    NotFound {
        /// Hex-encoded CID that was requested.
        cid: String,
    },

    /// A lower-level filesystem I/O error.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// The underlying filesystem is full and the object cannot be written.
    #[error("store is full — no space left on device")]
    StoreFull,

    /// A CID string could not be parsed (e.g. wrong hex length).
    #[error("invalid CID '{value}': {reason}")]
    InvalidCid {
        /// The raw string that failed to parse.
        value: String,
        /// The reason it failed.
        reason: String,
    },
}

/// Convenience alias used throughout this crate.
pub type Result<T> = std::result::Result<T, ObjError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_violation_display() {
        let err = ObjError::IntegrityViolation {
            cid: "deadbeef".into(),
            msg: "hash mismatch".into(),
        };
        let s = err.to_string();
        assert!(s.contains("deadbeef"), "display should include CID");
        assert!(
            s.contains("hash mismatch"),
            "display should include message"
        );
    }

    #[test]
    fn not_found_display() {
        let err = ObjError::NotFound {
            cid: "abc123".into(),
        };
        assert!(err.to_string().contains("abc123"));
    }

    #[test]
    fn store_full_display() {
        let err = ObjError::StoreFull;
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn io_error_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let obj_err: ObjError = io_err.into();
        assert!(obj_err.to_string().contains("denied"));
    }
}
