//! Error types for CraftSQL.
//!
//! All fallible CraftSQL operations return [`SqlError`] or the crate-local
//! [`Result`] alias.

use thiserror::Error;

/// Errors that can arise within CraftSQL.
#[derive(Debug, Error)]
pub enum SqlError {
    /// A write was attempted by a node that does not own the database.
    #[error("write rejected: writer {writer} is not the database owner {owner}")]
    UnauthorizedWriter { writer: String, owner: String },

    /// An Ed25519 signature on a signed-write message was invalid.
    #[error("invalid signature on write message from {writer}")]
    InvalidSignature { writer: String },

    /// The CAS root CID supplied by the caller does not match the current root.
    ///
    /// This is a compare-and-swap conflict: the caller's view is stale.
    #[error("CAS conflict: expected root {expected}, got {actual}")]
    CasConflict { expected: String, actual: String },

    /// The underlying CID-VFS layer returned an error.
    #[error("VFS error: {0}")]
    VfsError(#[from] craftec_vfs::VfsError),

    /// The SQL statement was syntactically or semantically invalid.
    #[error("SQL error: {0}")]
    SqlSyntaxError(String),

    /// A schema migration failed.
    #[error("schema migration failed: {0}")]
    MigrationFailed(String),

    /// The database has not been initialised (no root CID).
    #[error("database not initialised")]
    NotInitialised,

    /// An attempt was made to create a database that already exists.
    #[error("database already exists for owner {0}")]
    AlreadyExists(String),

    /// Serialisation or deserialisation error.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// libsql database engine error.
    #[error("libsql error: {0}")]
    LibsqlError(String),

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Crate-local result alias.
pub type Result<T, E = SqlError> = std::result::Result<T, E>;
