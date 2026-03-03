//! Error types for the Craftec system.
//!
//! All fallible operations in Craftec return [`CraftecError`] or the
//! local [`Result`] type alias.

use thiserror::Error;

/// The unified error type for all Craftec operations.
#[derive(Debug, Error)]
pub enum CraftecError {
    /// A failure in the storage layer (reading/writing pieces or the database).
    #[error("storage error: {0}")]
    StorageError(String),

    /// A network-level failure (connection, send, receive).
    #[error("network error: {0}")]
    NetworkError(String),

    /// An error in RLNC erasure coding or decoding.
    #[error("coding error: {0}")]
    CodingError(String),

    /// An error related to node identity, keypair operations, or signature
    /// verification.
    #[error("identity error: {0}")]
    IdentityError(String),

    /// A database-layer error (e.g. sled, SQLite).
    #[error("database error: {0}")]
    DatabaseError(String),

    /// An error originating in a WebAssembly guest module.
    #[error("wasm error: {0}")]
    WasmError(String),

    /// A transparent wrapper around [`std::io::Error`].
    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),

    /// An error while serializing or deserializing data.
    #[error("serialization error: {0}")]
    SerializationError(String),
}

/// Convenience `Result` alias using [`CraftecError`].
pub type Result<T> = std::result::Result<T, CraftecError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_storage_error() {
        let e = CraftecError::StorageError("disk full".into());
        assert_eq!(e.to_string(), "storage error: disk full");
    }

    #[test]
    fn display_identity_error() {
        let e = CraftecError::IdentityError("bad signature".into());
        assert_eq!(e.to_string(), "identity error: bad signature");
    }

    #[test]
    fn from_io_error() {
        use std::io;
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file missing");
        let craftec_err: CraftecError = io_err.into();
        assert!(matches!(craftec_err, CraftecError::IoError(_)));
    }
}
