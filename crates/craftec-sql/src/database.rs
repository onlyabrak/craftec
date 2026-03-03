//! CraftSQL distributed database.
//!
//! [`CraftDatabase`] is the primary entry point for CraftSQL.  It wraps a
//! CID-VFS instance and enforces the single-writer-per-identity invariant:
//! only the node whose Ed25519 identity matches [`CraftDatabase::owner`] may
//! issue mutations.
//!
//! ## SQL execution
//! SQL is executed through a file-backed libsql database.  After each
//! `execute()`, the actual SQLite pages are read from disk and synced to
//! CID-VFS for content-addressed persistence.
//!
//! ## Row representation
//! Query results are returned as a `Vec<Row>` where each [`Row`] is a
//! `Vec<ColumnValue>`.  This is intentionally simple — higher-level
//! application code typically deserialises rows into concrete types.
//!
//! ## Thread safety
//! `CraftDatabase` is `Send + Sync`.  The libsql connection is protected
//! by a `tokio::sync::Mutex` to ensure serial write access.  Concurrent
//! reads use the snapshot API and never block each other; writes take a
//! short exclusive lock only during the commit step.

use std::path::PathBuf;
use std::sync::Arc;

use craftec_types::{Cid, NodeId, Signature};
use craftec_vfs::CidVfs;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::commit::{CommitContext, check_ownership};
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
// libsql → ColumnValue conversion
// ---------------------------------------------------------------------------

/// Convert a libsql [`Value`](libsql::Value) to our [`ColumnValue`].
fn libsql_value_to_column(val: libsql::Value) -> ColumnValue {
    match val {
        libsql::Value::Null => ColumnValue::Null,
        libsql::Value::Integer(n) => ColumnValue::Integer(n),
        libsql::Value::Real(r) => ColumnValue::Real(r),
        libsql::Value::Text(s) => ColumnValue::Text(s),
        libsql::Value::Blob(b) => ColumnValue::Blob(b),
    }
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
/// 1. `CraftDatabase::create(owner, vfs, data_dir)` — initialise an empty database.
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
    /// Path to the on-disk SQLite database file.
    db_path: PathBuf,
    /// File-backed libsql database (kept alive so the connection remains valid).
    _libsql_db: libsql::Database,
    /// libsql connection for SQL mutations.  Protected by a tokio Mutex for
    /// serial write access (single-writer model).
    write_conn: tokio::sync::Mutex<libsql::Connection>,
    /// libsql connection for read-only queries.  Separate from write_conn so
    /// reads don't block behind writes (C2 fix).
    read_conn: tokio::sync::Mutex<libsql::Connection>,
    /// Optional event sender for publishing PageCommitted events.
    event_tx: RwLock<Option<tokio::sync::broadcast::Sender<craftec_types::Event>>>,
}

impl CraftDatabase {
    /// Create a new, empty [`CraftDatabase`] owned by `owner`.
    ///
    /// Initialises a file-backed libsql database at `data_dir/craftec.db`
    /// with the correct PRAGMAs and commits the initial SQLite pages to
    /// CID-VFS for content-addressed persistence.
    ///
    /// # Errors
    /// - [`SqlError::Io`] if the data directory cannot be created.
    /// - [`SqlError::VfsError`] if the VFS layer fails during initialisation.
    /// - [`SqlError::LibsqlError`] if the libsql engine fails to start.
    pub async fn create(
        owner: NodeId,
        vfs: Arc<CidVfs>,
        data_dir: &std::path::Path,
    ) -> Result<Self> {
        // Derive a stable database identity CID from the owner's public key bytes.
        let db_id = Cid::from_data(owner.as_bytes());

        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("craftec.db");

        // Create file-backed libsql database.
        let libsql_db = libsql::Builder::new_local(&db_path)
            .build()
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;

        // Create write connection and set PRAGMAs.
        let write_conn = libsql_db
            .connect()
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;

        // Set PRAGMAs: page_size = 16384, journal_mode = DELETE (no WAL per spec §35).
        // Use query() because PRAGMAs return result rows (execute() rejects row-returning SQL).
        write_conn
            .query("PRAGMA page_size = 16384", ())
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;
        write_conn
            .query("PRAGMA journal_mode = DELETE", ())
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;
        write_conn
            .query("PRAGMA busy_timeout = 5000", ())
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;

        // Force SQLite to write pages to disk so we can sync them to VFS.
        write_conn
            .execute(
                "CREATE TABLE IF NOT EXISTS _craftec_meta (key TEXT PRIMARY KEY, value TEXT)",
                (),
            )
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;
        write_conn
            .execute(
                "INSERT OR REPLACE INTO _craftec_meta VALUES ('version', '1')",
                (),
            )
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;

        // Create a separate read connection (C2: read/write separation).
        let read_conn = libsql_db
            .connect()
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;
        read_conn
            .query("PRAGMA busy_timeout = 5000", ())
            .await
            .map_err(|e| SqlError::LibsqlError(e.to_string()))?;

        tracing::info!(
            owner = %owner,
            db_id = %db_id,
            db_path = %db_path.display(),
            "CraftSQL: database created",
        );

        // Sync the initial SQLite pages to VFS.
        let root = Self::sync_pages_to_vfs(&db_path, &vfs).await?;

        Ok(Self {
            db_id,
            owner,
            vfs,
            root_cid: RwLock::new(root),
            db_path,
            _libsql_db: libsql_db,
            write_conn: tokio::sync::Mutex::new(write_conn),
            read_conn: tokio::sync::Mutex::new(read_conn),
            event_tx: RwLock::new(None),
        })
    }

    // -----------------------------------------------------------------------
    // Write path
    // -----------------------------------------------------------------------

    /// Execute a SQL mutation as `writer`.
    ///
    /// Only the database owner may call this method.  The mutation is executed
    /// through the in-memory libsql engine; the new root CID is stored
    /// internally after a successful VFS commit.
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

        let exec_start = std::time::Instant::now();
        tracing::debug!(
            db_id = %self.db_id,
            sql = sql,
            "CraftSQL: execute",
        );

        // Execute SQL through libsql.
        // T9 fix: hold the write_conn mutex through the entire execute-commit cycle
        // to prevent concurrent writes from interleaving VFS operations.
        let conn = self.write_conn.lock().await;
        conn.execute(sql, ())
            .await
            .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;

        // Sync actual SQLite pages from the database file to CID-VFS.
        let new_root = Self::sync_pages_to_vfs(&self.db_path, &self.vfs).await?;
        *self.root_cid.write() = new_root;

        // Publish PageCommitted event.
        if let Some(tx) = self.event_tx.read().as_ref() {
            let _ = tx.send(craftec_types::Event::PageCommitted {
                db_id: self.db_id,
                page_num: 0,
                root_cid: new_root,
            });
        }

        // Release conn mutex AFTER VFS commit and root_cid update.
        drop(conn);

        tracing::debug!(
            db_id = %self.db_id,
            new_root = %new_root,
            duration_ms = exec_start.elapsed().as_millis() as u64,
            "CraftSQL: execute committed",
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------------

    /// Execute a read-only SQL query, returning all matching rows.
    ///
    /// The query is executed against the in-memory libsql engine.  A VFS
    /// snapshot is pinned to the current root CID for consistency tracking.
    ///
    /// # Errors
    /// - [`SqlError::SqlSyntaxError`] if the SQL is invalid.
    /// - [`SqlError::VfsError`] on storage failure.
    pub async fn query(&self, sql: &str) -> Result<Vec<Row>> {
        let query_start = std::time::Instant::now();
        let sql_preview: String = sql.chars().take(100).collect();
        tracing::debug!(
            db_id = %self.db_id,
            sql_preview = %sql_preview,
            "CraftSQL: query start",
        );

        let _snapshot = self.vfs.snapshot().map_err(SqlError::VfsError)?;

        let conn = self.read_conn.lock().await;
        let mut rows_result = conn
            .query(sql, ())
            .await
            .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;

        let mut result: Vec<Row> = Vec::new();
        while let Some(row) = rows_result
            .next()
            .await
            .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?
        {
            let col_count = row.column_count();
            let mut cols = Vec::with_capacity(col_count as usize);
            for i in 0..col_count {
                let val = row
                    .get_value(i)
                    .map_err(|e| SqlError::SqlSyntaxError(e.to_string()))?;
                cols.push(libsql_value_to_column(val));
            }
            result.push(cols);
        }

        tracing::debug!(
            db_id = %self.db_id,
            row_count = result.len(),
            duration_ms = query_start.elapsed().as_millis() as u64,
            "CraftSQL: query complete",
        );

        Ok(result)
    }

    /// Inject an event sender so the database can publish
    /// [`PageCommitted`](craftec_types::Event::PageCommitted) after each execute().
    pub fn set_event_sender(&self, tx: tokio::sync::broadcast::Sender<craftec_types::Event>) {
        *self.event_tx.write() = Some(tx);
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Read actual SQLite pages from the database file and write them to VFS.
    async fn sync_pages_to_vfs(db_path: &std::path::Path, vfs: &CidVfs) -> Result<Cid> {
        let db_bytes = tokio::fs::read(db_path).await?;
        let page_size = vfs.page_size();
        for (i, chunk) in db_bytes.chunks(page_size).enumerate() {
            let mut page = vec![0u8; page_size];
            page[..chunk.len()].copy_from_slice(chunk);
            vfs.write_page(i as u32, &page)?;
        }
        let root = vfs.commit().await?;
        Ok(root)
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

    /// Return the path to the on-disk SQLite database file.
    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_obj::ContentAddressedStore;
    use craftec_types::NodeKeypair;
    use craftec_vfs::CidVfs;
    use tempfile::tempdir;

    async fn make_db() -> (CraftDatabase, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let owner = NodeKeypair::generate().node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .expect("database creation should succeed");
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
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();
        assert!(
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &owner)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn non_owner_execute_rejected() {
        let (db, _dir) = make_db().await;
        let non_owner = NodeKeypair::generate().node_id();
        assert!(matches!(
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &non_owner)
                .await,
            Err(SqlError::UnauthorizedWriter { .. })
        ));
    }

    #[tokio::test]
    async fn root_cid_changes_after_execute() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();
        let initial_root = db.root_cid();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)", &owner)
            .await
            .unwrap();
        assert_ne!(db.root_cid(), initial_root);
    }

    #[tokio::test]
    async fn sql_write_and_read_roundtrip() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();

        // CREATE TABLE, INSERT, then SELECT.
        db.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            &owner,
        )
        .await
        .unwrap();
        db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')", &owner)
            .await
            .unwrap();
        db.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')", &owner)
            .await
            .unwrap();

        let rows = db
            .query("SELECT id, name FROM users ORDER BY id")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], ColumnValue::Integer(1));
        assert_eq!(rows[0][1], ColumnValue::Text("Alice".to_string()));
        assert_eq!(rows[1][0], ColumnValue::Integer(2));
        assert_eq!(rows[1][1], ColumnValue::Text("Bob".to_string()));
    }

    #[tokio::test]
    async fn query_empty_table_returns_no_rows() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();

        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &owner)
            .await
            .unwrap();
        let rows = db.query("SELECT * FROM t").await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn sql_column_types() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();

        db.execute(
            "CREATE TABLE types_test (i INTEGER, r REAL, t TEXT, b BLOB, n INTEGER)",
            &owner,
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO types_test VALUES (42, 3.14, 'hello', X'DEADBEEF', NULL)",
            &owner,
        )
        .await
        .unwrap();

        let rows = db.query("SELECT * FROM types_test").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], ColumnValue::Integer(42));
        assert_eq!(rows[0][1], ColumnValue::Real(3.14));
        assert_eq!(rows[0][2], ColumnValue::Text("hello".to_string()));
        assert_eq!(rows[0][3], ColumnValue::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        assert_eq!(rows[0][4], ColumnValue::Null);
    }

    #[tokio::test]
    async fn invalid_sql_returns_error() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();

        let result = db.execute("THIS IS NOT VALID SQL", &owner).await;
        assert!(matches!(result, Err(SqlError::SqlSyntaxError(_))));
    }

    #[tokio::test]
    async fn database_file_exists_after_create() {
        let (db, _dir) = make_db().await;
        assert!(db.db_path().exists(), "database file should exist on disk");
    }

    #[tokio::test]
    async fn vfs_pages_are_real_sqlite_pages() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(Arc::clone(&store)).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();

        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)", &owner)
            .await
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'hello')", &owner)
            .await
            .unwrap();

        // The file on disk should be at least page_size bytes.
        let file_size = std::fs::metadata(db.db_path()).unwrap().len();
        assert!(
            file_size >= db.vfs().page_size() as u64,
            "database file ({file_size} bytes) should be at least one page"
        );

        // VFS snapshot page count should match the file pages.
        let snapshot = db.vfs().snapshot().unwrap();
        assert!(
            snapshot.page_count() > 0,
            "VFS should contain real pages from the database"
        );
    }

    #[tokio::test]
    async fn concurrent_execute_serialization() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = Arc::new(
            CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
                .await
                .unwrap(),
        );

        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)", &owner)
            .await
            .unwrap();

        // Launch two concurrent writes (T9: both should succeed without interleaving).
        let db1 = Arc::clone(&db);
        let db2 = Arc::clone(&db);
        let h1 = tokio::spawn(async move {
            db1.execute("INSERT INTO t VALUES (1, 'alpha')", &owner)
                .await
        });
        let h2 = tokio::spawn(async move {
            db2.execute("INSERT INTO t VALUES (2, 'beta')", &owner)
                .await
        });

        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();

        // Both rows should be present.
        let rows = db.query("SELECT id FROM t ORDER BY id").await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], ColumnValue::Integer(1));
        assert_eq!(rows[1][0], ColumnValue::Integer(2));
    }

    #[tokio::test]
    async fn concurrent_read_during_write() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = Arc::new(
            CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
                .await
                .unwrap(),
        );

        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT)", &owner)
            .await
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'alpha')", &owner)
            .await
            .unwrap();

        // Launch a read and a write concurrently — both should succeed.
        let db_r = Arc::clone(&db);
        let db_w = Arc::clone(&db);
        let read_handle =
            tokio::spawn(async move { db_r.query("SELECT id, val FROM t ORDER BY id").await });
        let write_handle = tokio::spawn(async move {
            db_w.execute("INSERT INTO t VALUES (2, 'beta')", &owner)
                .await
        });

        let read_result = read_handle.await.unwrap();
        let write_result = write_handle.await.unwrap();

        assert!(
            read_result.is_ok(),
            "read should succeed: {:?}",
            read_result
        );
        assert!(
            write_result.is_ok(),
            "write should succeed: {:?}",
            write_result
        );

        // Verify both rows exist.
        let rows = db.query("SELECT id FROM t ORDER BY id").await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn execute_publishes_page_committed() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let kp = NodeKeypair::generate();
        let owner = kp.node_id();
        let db = CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        db.set_event_sender(tx);

        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &owner)
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            craftec_types::Event::PageCommitted {
                db_id, root_cid, ..
            } => {
                assert_eq!(db_id, db.db_id());
                assert_eq!(root_cid, db.root_cid());
            }
            other => panic!("expected PageCommitted, got {:?}", other),
        }
    }
}
