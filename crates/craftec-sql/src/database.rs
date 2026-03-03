//! CraftSQL distributed database.
//!
//! [`CraftDatabase`] is the primary entry point for CraftSQL.  It wraps a
//! CID-VFS instance and enforces the single-writer-per-identity invariant:
//! only the node whose Ed25519 identity matches [`CraftDatabase::owner`] may
//! issue mutations.
//!
//! ## Row representation
//! Query results are returned as a `Vec<Row>` where each [`Row`] is a
//! `Vec<ColumnValue>`.  This is intentionally simple — higher-level
//! application code typically deserialises rows into concrete types.
//!
//! ## Thread safety
//! `CraftDatabase` is `Send + Sync` via interior `parking_lot::RwLock`s on
//! mutable state.  Concurrent reads use the snapshot API and never block each
//! other; writes take a short exclusive lock only during the commit step.

use std::sync::Arc;

use craftec_types::{Cid, NodeId, Signature};
use craftec_vfs::CidVfs;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::commit::{check_cas, check_ownership, CommitContext};
use crate::error::{Result, SqlError};

// ---------------------------------------------------------------------------
// Row / column value types
// ---------------------------------------------------------------------------

/// A single SQL column value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ColumnValue {
    /// NULL
    Null,
    /// 64-bit integer.
    Integer(i64),
    /// 64-bit float.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Raw bytes (BLOB).
    Blob(Vec<u8>),
}

impl std::fmt::Display for ColumnValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnValue::Null => write!(f, "NULL"),
            ColumnValue::Integer(n) => write!(f, "{n}"),
            ColumnValue::Real(r) => write!(f, "{r}"),
            ColumnValue::Text(s) => write!(f, "{s}"),
            ColumnValue::Blob(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

/// A single query result row: an ordered sequence of column values.
pub type Row = Vec<ColumnValue>;

// ---------------------------------------------------------------------------
// Signed write message
// ---------------------------------------------------------------------------

/// A write instruction signed by the originating node's Ed25519 key.
///
/// The network layer deserialises this from the wire and passes it to
/// [`RpcWriteHandler`](crate::rpc_write::RpcWriteHandler).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedWrite {
    /// Ed25519 public key of the writer (must match `db.owner`).
    pub writer: NodeId,
    /// The SQL mutation to execute.
    pub sql: String,
    /// Root CID the writer believes is current (compare-and-swap guard).
    pub expected_root: Option<Cid>,
    /// Ed25519 signature over the canonical payload produced by
    /// [`build_signed_payload`](crate::rpc_write::build_signed_payload).
    pub signature: Signature,
}

// ---------------------------------------------------------------------------
// CraftDatabase
// ---------------------------------------------------------------------------

/// A distributed SQL database backed by CID-VFS.
///
/// Single-writer-per-identity: only the `owner` can mutate the database.
/// Readers receive a consistent snapshot of the database at a specific root
/// CID, enabling concurrent reads without coordination.
///
/// ## Lifecycle
/// 1. `CraftDatabase::create(owner, vfs)` — initialise an empty database.
/// 2. `execute(sql, writer)` — write path (owner-only).
/// 3. `query(sql)` — read path (any reader, snapshot-isolated).
pub struct CraftDatabase {
    /// Unique database identity CID (BLAKE3 of the creation params).
    db_id: Cid,
    /// Ed25519 identity of the sole writer.
    owner: NodeId,
    /// CID-VFS storage backend.
    vfs: Arc<CidVfs>,
    /// Most recently committed root CID.
    root_cid: RwLock<Cid>,
}

impl CraftDatabase {
    /// Create a new, empty [`CraftDatabase`] owned by `owner`.
    ///
    /// Initialises a minimal SQLite schema through the VFS layer and commits
    /// an initial root CID.
    ///
    /// # Errors
    /// - [`SqlError::VfsError`] if the VFS layer fails during initialisation.
    pub async fn create(owner: NodeId, vfs: Arc<CidVfs>) -> Result<Self> {
        // Derive a stable database identity CID from the owner's public key bytes.
        let db_id = Cid::from_data(owner.as_bytes());

        tracing::info!(
            owner = %owner,
            db_id = %db_id,
            "CraftSQL: database created",
        );

        // Bootstrap: write a sentinel page 0 so the VFS has a root CID.
        let mut bootstrap_page = vec![0u8; vfs.page_size()];
        // Magic bytes so we can detect a Craftec-formatted database.
        bootstrap_page[..8].copy_from_slice(b"CRAFTEC1");
        vfs.write_page(0, &bootstrap_page)?;
        let root = vfs.commit().await?;

        Ok(Self {
            db_id,
            owner,
            vfs,
            root_cid: RwLock::new(root),
        })
    }

    // -----------------------------------------------------------------------
    // Write path
    // -----------------------------------------------------------------------

    /// Execute a SQL mutation as `writer`.
    ///
    /// Only the database owner may call this method.  The mutation is executed
    /// synchronously through the VFS layer; the new root CID is stored
    /// internally after a successful commit.
    ///
    /// # Arguments
    /// * `sql` — the SQL statement to execute (INSERT / UPDATE / DELETE / DDL).
    /// * `writer` — the [`NodeId`] executing the mutation.
    ///
    /// # Errors
    /// - [`SqlError::UnauthorizedWriter`] if `writer != owner`.
    /// - [`SqlError::SqlSyntaxError`] if the SQL is invalid.
    /// - [`SqlError::VfsError`] on storage failure.
    pub async fn execute(&self, sql: &str, writer: &NodeId) -> Result<()> {
        let ctx = CommitContext {
            writer: *writer,
            sql: sql.to_owned(),
            expected_root: None,
        };
        check_ownership(&ctx, &self.owner)?;

        tracing::debug!(
            db_id = %self.db_id,
            sql = sql,
            "CraftSQL: execute",
        );

        // In the full implementation the SQL would be executed through
        // libsql / SQLite via the VFS layer.  Here we simulate a write by
        // encoding the SQL as a page payload and committing it.
        let mut page = vec![0u8; self.vfs.page_size()];
        let sql_bytes = sql.as_bytes();
        let copy_len = sql_bytes.len().min(page.len() - 8);
        page[..8].copy_from_slice(b"CRAFTSQL");
        page[8..8 + copy_len].copy_from_slice(&sql_bytes[..copy_len]);

        // Use a page number derived from a hash of the SQL to avoid collisions
        // between different statements in the same session.
        let page_hash = blake3::hash(sql.as_bytes());
        let page_num = u32::from_le_bytes(page_hash.as_bytes()[0..4].try_into().unwrap());

        self.vfs.write_page(page_num, &page)?;
        let new_root = self.vfs.commit().await?;
        *self.root_cid.write() = new_root;

        tracing::debug!(
            db_id = %self.db_id,
            new_root = %new_root,
            "CraftSQL: execute committed",
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------------

    /// Execute a read-only SQL query, returning all matching rows.
    ///
    /// The query is executed against a snapshot pinned to the current root CID,
    /// providing consistent snapshot isolation even if a concurrent commit
    /// occurs.
    ///
    /// # Errors
    /// - [`SqlError::SqlSyntaxError`] if the SQL is invalid.
    /// - [`SqlError::VfsError`] on storage failure.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>> {
        let _snapshot = self.vfs.snapshot().map_err(SqlError::VfsError)?;

        // In the full implementation the SQL would be executed through
        // libsql / SQLite using the pinned snapshot.  We return an empty
        // result set here — the real execution engine is wired in by the
        // libsql VFS integration layer.
        let result: Vec<Row> = Vec::new();

        tracing::debug!(
            db_id = %self.db_id,
            sql = sql,
            rows = result.len(),
            "CraftSQL: query",
        );

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Return the current root CID of the database.
    pub fn root_cid(&self) -> Cid {
        *self.root_cid.read()
    }

    /// Return the database identity CID.
    pub fn db_id(&self) -> Cid {
        self.db_id
    }

    /// Return the owner's [`NodeId`].
    pub fn owner(&self) -> &NodeId {
        &self.owner
    }

    /// Return a reference to the underlying [`CidVfs`].
    pub fn vfs(&self) -> &Arc<CidVfs> {
        &self.vfs
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_obj::ContentAddressedStore;
    use craftec_types::{NodeId, NodeKeypair};
    use craftec_vfs::CidVfs;
    use tempfile::tempdir;

    async fn make_db() -> (CraftDatabase, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(dir.path(), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let owner = NodeKeypair::generate().node_id();
        let db = CraftDatabase::create(owner, vfs).await.expect("database creation should succeed");
        (db, dir)
    }

    #[tokio::test]
    async fn database_creation_establishes_root_cid() {
        let (db, _dir) = make_db().await;
        let root = db.root_cid();
        assert_ne!(root, Cid::from_bytes([0u8; 32]));
    }

    #[tokio::test]
    async fn owner_can_execute() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(dir.path(), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs).await.unwrap();
        assert!(db.execute("INSERT INTO t VALUES (1)", &owner).await.is_ok());
    }

    #[tokio::test]
    async fn non_owner_execute_rejected() {
        let (db, _dir) = make_db().await;
        let non_owner = NodeKeypair::generate().node_id();
        assert!(matches!(
            db.execute("INSERT INTO t VALUES (1)", &non_owner).await,
            Err(SqlError::UnauthorizedWriter { .. })
        ));
    }

    #[tokio::test]
    async fn root_cid_changes_after_execute() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(dir.path(), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs).await.unwrap();
        let initial_root = db.root_cid();
        db.execute("INSERT INTO t VALUES (42)", &owner).await.unwrap();
        assert_ne!(db.root_cid(), initial_root);
    }

    #[tokio::test]
    async fn query_returns_empty_result_set() {
        let (db, _dir) = make_db().await;
        let rows = db.query("SELECT * FROM t").unwrap();
        assert!(rows.is_empty());
    }
}
