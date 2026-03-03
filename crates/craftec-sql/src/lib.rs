//! `craftec-sql` — CraftSQL: distributed SQLite backed by CID-VFS.
//!
//! CraftSQL provides a SQLite-compatible distributed database with the
//! following key properties:
//!
//! ## Single-writer-per-identity
//! Each database is owned by an Ed25519 identity ([`NodeId`]).  Only that
//! identity can issue mutations.  This eliminates all write-write conflicts
//! without CRDT or distributed coordination — there is simply no race because
//! only one writer exists per database.
//!
//! ## CID-VFS storage backend
//! SQL data lives in SQLite pages stored as content-addressed objects in
//! CraftOBJ.  Each commit produces a new root CID that uniquely identifies
//! the complete database state.
//!
//! ## Snapshot isolation for reads
//! Any node can read a database by pinning a root CID snapshot.  Reads never
//! block writes and vice versa.
//!
//! ## Schema migrations
//! Because there is only one writer, `ALTER TABLE` and other DDL operations
//! are safe without migration locking.
//!
//! ## Crate layout
//! | Module | Responsibility |
//! |---|---|
//! | [`database`] | [`CraftDatabase`] — create / execute / query |
//! | [`schema`] | Schema migration helper |
//! | [`commit`] | Synchronous commit pipeline (steps 1–7) |
//! | [`rpc_write`] | [`RpcWriteHandler`] — handle signed writes from remote nodes |
//! | [`error`] | [`SqlError`] enum and [`Result`] alias |
//!
//! [`NodeId`]: craftec_types::NodeId

pub mod commit;
pub mod database;
pub mod error;
pub mod rpc_write;
pub mod schema;

// Convenience re-exports.
pub use database::{ColumnValue, CraftDatabase, Row, SignedWrite};
pub use error::{Result, SqlError};
pub use rpc_write::RpcWriteHandler;
pub use schema::{migrate, validate_migration_sql};
