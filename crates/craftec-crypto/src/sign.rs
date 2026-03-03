//! Persistent Ed25519 keypair management.
//!
//! [`KeyStore`] loads an existing keypair from `{data_dir}/node.key` on disk,
//! or generates a fresh one if no key file is found.  The key file contains
//! the raw 32-byte secret scalar — keep it confidential.

use std::path::{Path, PathBuf};

use craftec_types::{
    error::{CraftecError, Result},
    identity::{NodeId, NodeKeypair, Signature},
};
use tracing::{debug, trace};

/// Persistent Ed25519 signing key store.
///
/// The key is stored as raw secret bytes in `{data_dir}/node.key`.  On first
/// run a new keypair is generated with OS entropy and saved to disk.
pub struct KeyStore {
    keypair: NodeKeypair,
    key_path: PathBuf,
}

impl KeyStore {
    /// Load the keypair from `{data_dir}/node.key`, generating and saving a
    /// new one if the file does not exist.
    ///
    /// # Errors
    /// Returns [`CraftecError::IoError`] on read/write failures, or
    /// [`CraftecError::IdentityError`] if the key file is malformed.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let key_path = data_dir.join("node.key");
        debug!(key_path = %key_path.display(), "initialising KeyStore");

        let keypair = if key_path.exists() {
            debug!(key_path = %key_path.display(), "loading existing node keypair from disk");
            let bytes = std::fs::read(&key_path)?;
            if bytes.len() != 32 {
                return Err(CraftecError::IdentityError(format!(
                    "node.key must be 32 bytes, found {} bytes",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            NodeKeypair::from_secret_bytes(&arr)
        } else {
            debug!("no existing key found — generating new node keypair");
            std::fs::create_dir_all(data_dir)?;
            let kp = NodeKeypair::generate();
            std::fs::write(&key_path, kp.to_secret_bytes())?;
            debug!(
                key_path = %key_path.display(),
                node_id = %kp.node_id(),
                "saved new node keypair to disk"
            );
            kp
        };

        debug!(node_id = %keypair.node_id(), "KeyStore ready");
        Ok(Self { keypair, key_path })
    }

    /// Sign `msg` with the stored private key and return a [`Signature`].
    pub fn sign(&self, msg: &[u8]) -> Signature {
        trace!(msg_len = msg.len(), node_id = %self.keypair.node_id(), "signing message");
        let sig = self.keypair.sign(msg);
        debug!(node_id = %self.keypair.node_id(), "produced signature");
        sig
    }

    /// Verify that `sig` over `msg` was produced by `pubkey`.
    pub fn verify(&self, msg: &[u8], sig: &Signature, pubkey: &NodeId) -> bool {
        trace!(
            msg_len = msg.len(),
            node_id = %pubkey,
            "verifying signature"
        );
        let ok = craftec_types::identity::verify(msg, sig, pubkey);
        debug!(node_id = %pubkey, verified = ok, "signature verification result");
        ok
    }

    /// Return the [`NodeId`] for the managed keypair.
    pub fn node_id(&self) -> NodeId {
        self.keypair.node_id()
    }

    /// Return the path of the key file on disk.
    pub fn key_path(&self) -> &Path {
        &self.key_path
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_keypair_on_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let ks = KeyStore::new(dir.path()).unwrap();
        assert!(dir.path().join("node.key").exists());
        let _ = ks.node_id(); // must not panic
    }

    #[test]
    fn loads_existing_keypair() {
        let dir = tempfile::tempdir().unwrap();
        // First call generates.
        let id1 = KeyStore::new(dir.path()).unwrap().node_id();
        // Second call loads from disk.
        let id2 = KeyStore::new(dir.path()).unwrap().node_id();
        assert_eq!(id1, id2, "NodeId must be stable across reloads");
    }

    #[test]
    fn sign_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let ks = KeyStore::new(dir.path()).unwrap();
        let msg = b"test message for signing";
        let sig = ks.sign(msg);
        let node_id = ks.node_id();
        assert!(ks.verify(msg, &sig, &node_id));
    }

    #[test]
    fn verify_wrong_message_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ks = KeyStore::new(dir.path()).unwrap();
        let sig = ks.sign(b"original");
        assert!(!ks.verify(b"tampered", &sig, &ks.node_id()));
    }

    #[test]
    fn bad_key_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // Write a key file with the wrong length.
        std::fs::write(dir.path().join("node.key"), b"tooshort").unwrap();
        let result = KeyStore::new(dir.path());
        assert!(matches!(result, Err(CraftecError::IdentityError(_))));
    }
}
