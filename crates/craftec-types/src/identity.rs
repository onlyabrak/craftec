//! Node identity types: keypair management, signing, and node ID.
//!
//! Craftec uses Ed25519 keys (via `ed25519-dalek`) for node identity, following
//! the same convention as iroh: the 32-byte compressed public key *is* the
//! [`NodeId`].

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::error::{CraftecError, Result};

// ── NodeId ─────────────────────────────────────────────────────────────────

/// The 32-byte compressed Ed25519 public key that uniquely identifies a node.
///
/// This follows the iroh convention: `NodeId == public key bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId([u8; 32]);

impl NodeId {
    /// Create a `NodeId` from raw bytes.
    #[inline]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Generate a random `NodeId` (convenience for tests and internal use).
    pub fn generate() -> Self {
        NodeKeypair::generate().node_id()
    }

    /// Try to construct a `NodeId` from a byte slice.  Returns an error if the
    /// slice is not exactly 32 bytes.
    pub fn from_slice(b: &[u8]) -> Result<Self> {
        if b.len() != 32 {
            return Err(CraftecError::IdentityError(format!(
                "NodeId must be 32 bytes, got {}",
                b.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(b);
        Ok(Self(arr))
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

// ── Signature ─────────────────────────────────────────────────────────────

/// An Ed25519 signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature(ed25519_dalek::Signature);

impl Signature {
    /// Create a `Signature` from a raw `ed25519_dalek::Signature`.
    #[inline]
    pub fn from_dalek(sig: ed25519_dalek::Signature) -> Self {
        Self(sig)
    }

    /// Return the inner `ed25519_dalek::Signature`.
    #[inline]
    pub fn inner(&self) -> &ed25519_dalek::Signature {
        &self.0
    }

    /// Return the raw 64-byte representation.
    pub fn to_bytes(&self) -> [u8; 64] {
        self.0.to_bytes()
    }
}

// ── NodeKeypair ────────────────────────────────────────────────────────────

/// A node's Ed25519 signing keypair.
///
/// The keypair is generated with OS entropy and kept in memory. For persistent
/// key storage use `craftec_crypto::sign::KeyStore`.
pub struct NodeKeypair {
    signing_key: SigningKey,
}

impl NodeKeypair {
    /// Generate a new random keypair using OS entropy.
    pub fn generate() -> Self {
        debug!("generating new Ed25519 node keypair");
        let signing_key = SigningKey::generate(&mut OsRng);
        let kp = Self { signing_key };
        debug!(node_id = %kp.node_id(), "generated node keypair");
        kp
    }

    /// Construct from an existing `ed25519_dalek::SigningKey`.
    pub fn from_signing_key(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }

    /// Return the [`NodeId`] (compressed public key bytes) for this keypair.
    pub fn node_id(&self) -> NodeId {
        NodeId(self.signing_key.verifying_key().to_bytes())
    }

    /// Return the [`NodeId`] — alias for `node_id()`.
    #[inline]
    pub fn public_key(&self) -> NodeId {
        self.node_id()
    }

    /// Sign `msg` and return the [`Signature`].
    pub fn sign(&self, msg: &[u8]) -> Signature {
        trace!(msg_len = msg.len(), node_id = %self.node_id(), "signing message");
        let sig = self.signing_key.sign(msg);
        debug!(node_id = %self.node_id(), "produced signature");
        Signature::from_dalek(sig)
    }

    /// Export the raw 32-byte secret scalar for persistent storage.
    pub fn to_secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Reconstruct a keypair from 32 raw secret bytes.
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(bytes);
        Self { signing_key }
    }
}

// ── Free-function verify ───────────────────────────────────────────────────

/// Verify that `sig` over `msg` was produced by the key identified by
/// `node_id`.
///
/// Returns `true` on success, `false` if verification fails.
pub fn verify(msg: &[u8], sig: &Signature, node_id: &NodeId) -> bool {
    trace!(
        msg_len = msg.len(),
        node_id = %node_id,
        "verifying Ed25519 signature"
    );
    let verifying_key = match VerifyingKey::from_bytes(node_id.as_bytes()) {
        Ok(k) => k,
        Err(e) => {
            debug!(error = %e, "failed to parse verifying key from NodeId");
            return false;
        }
    };
    let ok = verifying_key.verify(msg, sig.inner()).is_ok();
    debug!(node_id = %node_id, verified = ok, "signature verification result");
    ok
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let kp = NodeKeypair::generate();
        let msg = b"hello craftec";
        let sig = kp.sign(msg);
        let node_id = kp.node_id();
        assert!(verify(msg, &sig, &node_id));
    }

    #[test]
    fn verify_wrong_message_fails() {
        let kp = NodeKeypair::generate();
        let sig = kp.sign(b"original");
        assert!(!verify(b"tampered", &sig, &kp.node_id()));
    }

    #[test]
    fn node_id_round_trip() {
        let kp = NodeKeypair::generate();
        let id = kp.node_id();
        let bytes = *id.as_bytes();
        let id2 = NodeId::from_bytes(bytes);
        assert_eq!(id, id2);
    }

    #[test]
    fn keypair_secret_round_trip() {
        let kp = NodeKeypair::generate();
        let id_before = kp.node_id();
        let secret = kp.to_secret_bytes();
        let kp2 = NodeKeypair::from_secret_bytes(&secret);
        assert_eq!(kp2.node_id(), id_before);
    }
}
