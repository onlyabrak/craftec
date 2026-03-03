//! RPC write path for CraftSQL.
//!
//! Remote nodes submit SQL mutations as [`SignedWrite`] messages over the
//! Craftec P2P network.  The [`RpcWriteHandler`] verifies the signature,
//! enforces ownership, performs a compare-and-swap root check, and executes
//! the mutation — producing a new root CID that is returned to the caller.
//!
//! ## Security model
//! - The `writer` field in [`SignedWrite`] is the Ed25519 public key.
//! - The signature covers `(writer_bytes, sql, expected_root)` encoded in a
//!   deterministic length-prefixed format.
//! - Verification uses `craftec_types::identity::verify`.
//! - If the writer is not the database owner, the request is rejected before
//!   any mutation occurs.
//!
//! ## Compare-and-swap
//! The `expected_root` field guards against lost-update scenarios when a
//! client has a stale view.  If `expected_root != current_root` the write is
//! rejected with [`SqlError::CasConflict`].  Clients should refresh their
//! root CID and retry.

use std::sync::Arc;

use craftec_types::{Cid, NodeId};

use crate::commit::{CommitContext, check_cas, check_ownership};
use crate::database::{CraftDatabase, SignedWrite};
use crate::error::{Result, SqlError};

/// Handles `SIGNED_WRITE` instructions from remote identities.
///
/// The RPC node reconstructs pages, executes the mutation through CID-VFS,
/// and returns the new root CID to the originating writer.
///
/// # Thread safety
/// `RpcWriteHandler` is `Send + Sync`.  Concurrent writes from the *same*
/// owner are serialised by the VFS dirty-page mutex; concurrent reads are
/// never blocked.
pub struct RpcWriteHandler {
    database: Arc<CraftDatabase>,
}

impl RpcWriteHandler {
    /// Create a new [`RpcWriteHandler`] backed by `database`.
    pub fn new(database: Arc<CraftDatabase>) -> Self {
        Self { database }
    }

    /// Handle a single signed write message.
    ///
    /// ## Steps
    /// 1. Verify Ed25519 signature (using writer's public key in the message).
    /// 2. Verify `writer == owner` (single-writer enforcement).
    /// 3. Compare-and-swap root CID check.
    /// 4. Execute the SQL mutation through CID-VFS.
    /// 5. Return the new root CID.
    ///
    /// # Errors
    /// - [`SqlError::InvalidSignature`] — signature verification failed.
    /// - [`SqlError::UnauthorizedWriter`] — writer is not the database owner.
    /// - [`SqlError::CasConflict`] — expected root does not match current.
    /// - [`SqlError::VfsError`] — storage layer failure.
    pub async fn handle_signed_write(&self, msg: &SignedWrite) -> Result<Cid> {
        // Step 1: verify Ed25519 signature.
        self.verify_signature(msg)?;

        // Step 2 & 3: ownership + CAS check.
        let ctx = CommitContext {
            writer: msg.writer,
            sql: msg.sql.clone(),
            expected_root: msg.expected_root,
        };
        if let Err(e) = check_ownership(&ctx, self.database.owner()) {
            tracing::warn!(
                writer = %msg.writer,
                owner = %self.database.owner(),
                "CraftSQL RPC: ownership check rejected"
            );
            return Err(e);
        }
        if let Err(e) = check_cas(&ctx, Some(self.database.root_cid())) {
            tracing::warn!(
                writer = %msg.writer,
                expected_root = ?msg.expected_root,
                actual_root = %self.database.root_cid(),
                "CraftSQL RPC: CAS conflict"
            );
            return Err(e);
        }

        // Step 4: execute the mutation.
        self.database.execute(&msg.sql, &msg.writer).await?;

        let new_root = self.database.root_cid();

        tracing::info!(
            writer = %msg.writer,
            new_root = %new_root,
            "CraftSQL: RPC write executed",
        );

        Ok(new_root)
    }

    /// Verify the Ed25519 signature carried in `msg`.
    ///
    /// Uses the free-function [`craftec_types::identity::verify`] to check
    /// that `msg.signature` was produced by the private key corresponding to
    /// `msg.writer`.
    ///
    /// # Errors
    /// Returns [`SqlError::InvalidSignature`] on any verification failure.
    fn verify_signature(&self, msg: &SignedWrite) -> Result<()> {
        let signed_payload = build_signed_payload(&msg.writer, &msg.sql, msg.expected_root);

        let ok = craftec_types::identity::verify(&signed_payload, &msg.signature, &msg.writer);
        if !ok {
            return Err(SqlError::InvalidSignature {
                writer: format!("{}", msg.writer),
            });
        }
        Ok(())
    }

    /// Return a reference to the underlying [`CraftDatabase`].
    pub fn database(&self) -> &Arc<CraftDatabase> {
        &self.database
    }
}

/// Build the canonical byte payload that the writer signs.
///
/// Format: length-prefixed concatenation of:
/// 1. `writer_pubkey_bytes` (32 bytes, preceded by a `u32 LE` length).
/// 2. `sql_bytes` (UTF-8, preceded by a `u32 LE` length).
/// 3. Option tag: `0x00` for `None`, `0x01` followed by 32 CID bytes for `Some`.
pub fn build_signed_payload(writer: &NodeId, sql: &str, root: Option<Cid>) -> Vec<u8> {
    let mut buf = Vec::new();
    let writer_bytes = writer.as_bytes();
    buf.extend_from_slice(&(writer_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(writer_bytes);
    let sql_bytes = sql.as_bytes();
    buf.extend_from_slice(&(sql_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(sql_bytes);
    match root {
        None => buf.push(0),
        Some(cid) => {
            buf.push(1);
            buf.extend_from_slice(cid.as_bytes());
        }
    }
    buf
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

    async fn make_handler() -> (RpcWriteHandler, NodeKeypair, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&dir.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(store).unwrap());
        let keypair = NodeKeypair::generate();
        let owner = keypair.node_id();
        let db = Arc::new(
            CraftDatabase::create(owner, vfs, &dir.path().join("sql"))
                .await
                .unwrap(),
        );
        (RpcWriteHandler::new(db), keypair, dir)
    }

    fn sign_write(keypair: &NodeKeypair, sql: &str, expected_root: Option<Cid>) -> SignedWrite {
        let writer = keypair.node_id();
        let payload = build_signed_payload(&writer, sql, expected_root);
        let signature = keypair.sign(&payload);
        SignedWrite {
            writer,
            sql: sql.to_owned(),
            expected_root,
            signature,
        }
    }

    #[tokio::test]
    async fn owner_write_succeeds() {
        let (handler, keypair, _dir) = make_handler().await;
        let current_root = handler.database().root_cid();
        let msg = sign_write(
            &keypair,
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
            Some(current_root),
        );
        let new_root = handler.handle_signed_write(&msg).await.unwrap();
        assert_ne!(new_root, current_root);
    }

    #[tokio::test]
    async fn invalid_signature_rejected() {
        let (handler, keypair, _dir) = make_handler().await;
        let other_keypair = NodeKeypair::generate();
        let writer = keypair.node_id();
        let current_root = handler.database().root_cid();
        let payload = build_signed_payload(&writer, "INSERT INTO t VALUES (1)", Some(current_root));
        // Sign with the *wrong* key.
        let bad_sig = other_keypair.sign(&payload);
        let msg = SignedWrite {
            writer,
            sql: "INSERT INTO t VALUES (1)".into(),
            expected_root: Some(current_root),
            signature: bad_sig,
        };
        assert!(matches!(
            handler.handle_signed_write(&msg).await,
            Err(SqlError::InvalidSignature { .. })
        ));
    }

    #[tokio::test]
    async fn non_owner_rejected() {
        let (handler, _owner_kp, _dir) = make_handler().await;
        let non_owner_kp = NodeKeypair::generate();
        let current_root = handler.database().root_cid();
        let msg = sign_write(
            &non_owner_kp,
            "INSERT INTO t VALUES (2)",
            Some(current_root),
        );
        assert!(matches!(
            handler.handle_signed_write(&msg).await,
            Err(SqlError::UnauthorizedWriter { .. })
        ));
    }

    #[tokio::test]
    async fn stale_root_cas_conflict() {
        let (handler, keypair, _dir) = make_handler().await;
        let stale_root = Cid::from_bytes([0xDE; 32]);
        let msg = sign_write(&keypair, "INSERT INTO t VALUES (3)", Some(stale_root));
        assert!(matches!(
            handler.handle_signed_write(&msg).await,
            Err(SqlError::CasConflict { .. })
        ));
    }
}
