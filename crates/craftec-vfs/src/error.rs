//! Error types for the CID-VFS layer.
//!
//! All fallible operations in `craftec-vfs` return [`VfsError`] or the
//! crate-local [`Result`] alias.

use thiserror::Error;

/// Errors that can arise within the CID-VFS layer.
#[derive(Debug, Error)]
pub enum VfsError {
    /// A page was requested but its CID could not be found in the page index.
    #[error("page {0} not found in page index")]
    PageNotFound(u32),

    /// The fetched page data failed BLAKE3 integrity verification.
    #[error("integrity check failed for page {page}: expected {expected}, got {actual}")]
    IntegrityCheckFailed {
        page: u32,
        expected: String,
        actual: String,
    },

    /// The underlying content-addressed store returned an error.
    #[error("CAS store error: {0}")]
    StoreError(String),

    /// Serialization or deserialization of the page index failed.
    #[error("page index serialization error: {0}")]
    SerializationError(String),

    /// A commit was attempted while dirty pages were being modified concurrently.
    #[error("concurrent commit conflict")]
    CommitConflict,

    /// The page size supplied is invalid (must be a power of two, 512 – 65536).
    #[error("invalid page size {0}; must be a power of two between 512 and 65536")]
    InvalidPageSize(usize),

    /// A snapshot was requested but no root CID is established yet (empty database).
    #[error("no root CID available; the database is empty")]
    NoRootCid,

    /// Generic I/O error propagated from an underlying layer.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Crate-local result alias.
pub type Result<T, E = VfsError> = std::result::Result<T, E>;
