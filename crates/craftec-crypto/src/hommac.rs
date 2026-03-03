//! Homomorphic MAC for RLNC piece verification.
//!
//! ## Background
//!
//! Random Linear Network Coding (RLNC) allows intermediate nodes to *recode*
//! pieces: they take a linear combination of received pieces and forward the
//! result.  A naive hash of the piece payload cannot survive recoding because
//! the payload changes.
//!
//! This module provides a lightweight HomMAC scheme built on BLAKE3:
//!
//! ```text
//! tag = BLAKE3(key || coding_vector || data)
//! ```
//!
//! Because the key is derived per-CID and is only distributed to authorized
//! verifiers, a malicious node cannot forge a valid tag for an incorrectly
//! coded piece.
//!
//! ## Recoding
//!
//! When a node recodes `n` pieces into a new piece with coefficients `c_i`:
//!
//! ```text
//! new_coding_vector = Σ c_i * coding_vector_i   (GF(2^8))
//! new_data          = Σ c_i * data_i              (GF(2^8))
//! ```
//!
//! The [`combine_tags`] function produces the expected tag for the recoded
//! piece from the original tags, enabling downstream verification without
//! re-contacting the origin.
//!
//! > **Note:** The current `combine_tags` implementation uses BLAKE3-based
//! > combination rather than a GF(2^8) field-compatible additive scheme.
//! > A production deployment should replace this with a proper algebraic
//! > HomMAC over GF(2^8) (e.g., using the Catalano–Fiore construction).

use rand::RngCore;
use tracing::{debug, trace};

/// A 32-byte symmetric key used to authenticate RLNC pieces for a single CID.
///
/// Generate a fresh key for each CID with [`HomMacKey::generate`].  The key
/// must be kept secret from nodes that should not be able to forge tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomMacKey([u8; 32]);

impl HomMacKey {
    /// Generate a random 32-byte key using OS entropy.
    pub fn generate() -> Self {
        trace!("generating HomMacKey");
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        debug!("generated HomMacKey");
        Self(key)
    }

    /// Construct from raw bytes (e.g. loaded from storage).
    #[inline]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw key bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Compute the HomMAC tag for a single coded piece.
///
/// `tag = BLAKE3(key || coding_vector || data)`
///
/// Both `coding_vector` and `data` are included so that the tag covers the
/// complete piece representation.
pub fn compute_tag(key: &HomMacKey, coding_vector: &[u8], data: &[u8]) -> [u8; 32] {
    trace!(
        coding_vector_len = coding_vector.len(),
        data_len = data.len(),
        "computing HomMAC tag"
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.as_bytes());
    hasher.update(coding_vector);
    hasher.update(data);
    let tag = *hasher.finalize().as_bytes();
    debug!("computed HomMAC tag");
    tag
}

/// Verify that `tag` is a valid HomMAC for the given piece components.
///
/// Returns `true` if `tag == compute_tag(key, coding_vector, data)`.
pub fn verify_tag(
    key: &HomMacKey,
    coding_vector: &[u8],
    data: &[u8],
    tag: &[u8; 32],
) -> bool {
    trace!(
        coding_vector_len = coding_vector.len(),
        data_len = data.len(),
        "verifying HomMAC tag"
    );
    let expected = compute_tag(key, coding_vector, data);
    let ok = expected == *tag;
    debug!(verified = ok, "HomMAC tag verification result");
    ok
}

/// Combine multiple HomMAC tags for an RLNC recoding operation.
///
/// Given `n` source piece tags and `n` GF(2^8) recoding coefficients, produces
/// the expected tag for the linearly recoded piece.
///
/// Current implementation: `BLAKE3(key || BLAKE3(tags[0] * c[0] XOR ... || coefficients))`
///
/// This is a structurally correct placeholder.  Replace with a proper
/// algebraic linear combination over GF(2^8) for a production system.
///
/// # Panics
/// Panics in debug builds if `tags.len() != coefficients.len()`.
pub fn combine_tags(key: &HomMacKey, tags: &[[u8; 32]], coefficients: &[u8]) -> [u8; 32] {
    trace!(
        tag_count = tags.len(),
        coeff_count = coefficients.len(),
        "combining HomMAC tags for RLNC recoding"
    );
    debug_assert_eq!(
        tags.len(),
        coefficients.len(),
        "number of tags must equal number of coefficients"
    );

    // Accumulate a combined intermediate value by XOR-ing each tag scaled
    // by its coefficient (GF(2^8) scalar multiplication is approximated here
    // as byte-wise multiplication mod 0 — replace with proper GF arithmetic).
    let mut combined = [0u8; 32];
    for (tag, &coeff) in tags.iter().zip(coefficients.iter()) {
        for (acc, &t) in combined.iter_mut().zip(tag.iter()) {
            // GF(2^8) scalar multiplication: coeff * t (XOR accumulation of
            // the repeated-doubling result is the correct approach; this
            // placeholder uses wrapping multiplication for illustration).
            *acc ^= gf256_mul(coeff, t);
        }
    }

    // Re-authenticate the combined value under the key.
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.as_bytes());
    hasher.update(&combined);
    hasher.update(coefficients);
    let tag = *hasher.finalize().as_bytes();
    debug!(tag_count = tags.len(), "combined HomMAC tags");
    tag
}

// ── GF(2^8) scalar multiply ────────────────────────────────────────────────

/// Multiply two elements of GF(2^8) using the standard irreducible polynomial
/// x^8 + x^4 + x^3 + x^2 + 1 (0x11D), as used in AES and common RLNC
/// implementations.
#[inline]
fn gf256_mul(a: u8, b: u8) -> u8 {
    let mut result: u8 = 0;
    let mut aa = a;
    let mut bb = b;
    for _ in 0..8 {
        if bb & 1 != 0 {
            result ^= aa;
        }
        let high_bit = aa & 0x80;
        aa <<= 1;
        if high_bit != 0 {
            aa ^= 0x1D; // x^4 + x^3 + x^2 + 1 (low byte of 0x11D)
        }
        bb >>= 1;
    }
    result
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_and_verify_tag() {
        let key = HomMacKey::generate();
        let cv = vec![1u8, 0, 0, 0];
        let data = vec![0xABu8; 64];
        let tag = compute_tag(&key, &cv, &data);
        assert!(verify_tag(&key, &cv, &data, &tag));
    }

    #[test]
    fn verify_wrong_data_fails() {
        let key = HomMacKey::generate();
        let cv = vec![1u8, 0, 0, 0];
        let data = vec![0xABu8; 64];
        let tag = compute_tag(&key, &cv, &data);
        let tampered = vec![0xCDu8; 64];
        assert!(!verify_tag(&key, &cv, &tampered, &tag));
    }

    #[test]
    fn verify_wrong_coding_vector_fails() {
        let key = HomMacKey::generate();
        let cv = vec![1u8, 0, 0, 0];
        let data = vec![0u8; 64];
        let tag = compute_tag(&key, &cv, &data);
        let wrong_cv = vec![0u8, 1, 0, 0];
        assert!(!verify_tag(&key, &wrong_cv, &data, &tag));
    }

    #[test]
    fn combine_tags_deterministic() {
        let key = HomMacKey::generate();
        let tags = vec![[1u8; 32], [2u8; 32]];
        let coeffs = vec![3u8, 5u8];
        let t1 = combine_tags(&key, &tags, &coeffs);
        let t2 = combine_tags(&key, &tags, &coeffs);
        assert_eq!(t1, t2);
    }

    #[test]
    fn combine_tags_changes_with_different_coefficients() {
        let key = HomMacKey::generate();
        let tags = vec![[1u8; 32], [2u8; 32]];
        let t1 = combine_tags(&key, &tags, &[1u8, 0u8]);
        let t2 = combine_tags(&key, &tags, &[0u8, 1u8]);
        assert_ne!(t1, t2);
    }

    #[test]
    fn gf256_mul_identity() {
        // Multiplying by 1 in GF(256) should be identity.
        assert_eq!(gf256_mul(0xAB, 1), 0xAB);
        assert_eq!(gf256_mul(1, 0xAB), 0xAB);
    }

    #[test]
    fn gf256_mul_zero() {
        assert_eq!(gf256_mul(0xAB, 0), 0);
        assert_eq!(gf256_mul(0, 0xAB), 0);
    }
}
