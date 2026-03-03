//! Homomorphic MAC for RLNC piece verification.
//!
//! ## Background
//!
//! Random Linear Network Coding (RLNC) allows intermediate nodes to *recode*
//! pieces: they take a linear combination of received pieces and forward the
//! result.  A naive hash of the piece payload cannot survive recoding because
//! the payload changes.
//!
//! This module provides a HomMAC scheme with the following properties:
//!
//! 1. **Unforgeability**: without the secret key, an adversary cannot produce
//!    a valid tag for any piece.
//! 2. **Homomorphism**: given tags for pieces `p_1, ..., p_n` and recoding
//!    coefficients `c_1, ..., c_n`, the tag for the recoded piece
//!    `Σ c_i * p_i` can be computed as `Σ c_i * tag(p_i)` — without
//!    knowing the original pieces or the key.
//!
//! ## Scheme
//!
//! For each tag byte position `j` (0..31), a pseudorandom vector `r_j` of
//! length `n` (= piece size) is derived from the key:
//!
//! ```text
//! r_j = BLAKE3_XOF(key || j)[0..n]
//! tag[j] = Σ_i r_j[i] * piece_bytes[i]   in GF(2^8)
//! ```
//!
//! where `piece_bytes = coding_vector || data`.
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
//! The [`combine_tags`] function computes `combined[j] = Σ c_i * tag_i[j]`
//! in GF(2^8).  By linearity of the inner product, this equals
//! `compute_tag(key, new_cv, new_data)` — enabling downstream verification
//! without re-contacting the origin.

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
/// For each tag byte position `j` (0..31), a pseudorandom coefficient vector
/// `r_j` is derived from the key via BLAKE3 XOF.  The tag byte is the GF(2^8)
/// inner product `Σ_i r_j[i] * piece[i]` where `piece = cv || data`.
///
/// This function is **linear** in the piece data, enabling homomorphic
/// combination via [`combine_tags`].
pub fn compute_tag(key: &HomMacKey, coding_vector: &[u8], data: &[u8]) -> [u8; 32] {
    trace!(
        coding_vector_len = coding_vector.len(),
        data_len = data.len(),
        "computing HomMAC tag"
    );
    let n = coding_vector.len() + data.len();
    let mut tag = [0u8; 32];

    for j in 0..32u8 {
        // Derive pseudorandom coefficients for tag position j.
        let mut hasher = blake3::Hasher::new();
        hasher.update(key.as_bytes());
        hasher.update(&[j]);
        let mut xof = hasher.finalize_xof();

        // GF(2^8) inner product: Σ r[i] * piece[i]
        let mut acc = 0u8;
        let mut r_buf = [0u8; 256];

        // Process coding_vector bytes.
        for cv_chunk in coding_vector.chunks(256) {
            xof.fill(&mut r_buf[..cv_chunk.len()]);
            for (r, cv) in r_buf.iter().zip(cv_chunk.iter()) {
                acc ^= gf256_mul(*r, *cv);
            }
        }

        // Process data bytes.
        for data_chunk in data.chunks(256) {
            xof.fill(&mut r_buf[..data_chunk.len()]);
            for (r, d) in r_buf.iter().zip(data_chunk.iter()) {
                acc ^= gf256_mul(*r, *d);
            }
        }

        tag[j as usize] = acc;
    }

    debug!(piece_len = n, "computed HomMAC tag");
    tag
}

/// Verify that `tag` is a valid HomMAC for the given piece components.
///
/// Returns `true` if `tag == compute_tag(key, coding_vector, data)`.
pub fn verify_tag(key: &HomMacKey, coding_vector: &[u8], data: &[u8], tag: &[u8; 32]) -> bool {
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
/// the expected tag for the linearly recoded piece:
///
/// ```text
/// combined[j] = Σ_i  coeff[i] * tag_i[j]    in GF(2^8)
/// ```
///
/// By linearity of the inner-product MAC, this equals
/// `compute_tag(key, recoded_cv, recoded_data)` for the recoded piece.
///
/// The `key` parameter is accepted for API uniformity but is not used
/// (the combination is purely algebraic).
///
/// # Panics
/// Panics in debug builds if `tags.len() != coefficients.len()`.
pub fn combine_tags(_key: &HomMacKey, tags: &[[u8; 32]], coefficients: &[u8]) -> [u8; 32] {
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

    let mut combined = [0u8; 32];
    for (tag, &coeff) in tags.iter().zip(coefficients.iter()) {
        for (acc, &t) in combined.iter_mut().zip(tag.iter()) {
            *acc ^= gf256_mul(coeff, t);
        }
    }

    debug!(tag_count = tags.len(), "combined HomMAC tags");
    combined
}

// ── GF(2^8) scalar multiply ────────────────────────────────────────────────

/// Multiply two elements of GF(2^8) using the AES irreducible polynomial
/// x^8 + x^4 + x^3 + x + 1 (0x11B), matching `craftec_rlnc::gf256`.
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
            aa ^= 0x1B; // x^4 + x^3 + x + 1 (low byte of 0x11B)
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

    #[test]
    fn gf256_mul_matches_rlnc_polynomial() {
        // Verify our GF(2^8) uses the AES polynomial (0x11B).
        // 0x02 * 0x80 = 0x1B (xtime of 0x80 with AES poly)
        assert_eq!(gf256_mul(0x02, 0x80), 0x1B);
        // Known inverse pair in AES field: 0x53 * 0xCA = 1
        assert_eq!(gf256_mul(0x53, 0xCA), 1);
    }

    /// The core HomMAC property: combining tags of original pieces with
    /// recoding coefficients yields the same tag as computing it directly
    /// on the recoded piece.
    #[test]
    fn combine_tags_is_homomorphic() {
        let key = HomMacKey::generate();

        // Two source pieces with k=4 coding vectors.
        let cv1 = vec![1u8, 0, 0, 0];
        let data1 = vec![0xAAu8; 64];

        let cv2 = vec![0u8, 1, 0, 0];
        let data2 = vec![0x55u8; 64];

        // Compute tags for original pieces.
        let tag1 = compute_tag(&key, &cv1, &data1);
        let tag2 = compute_tag(&key, &cv2, &data2);

        // Recoding coefficients.
        let c1 = 0x03u8;
        let c2 = 0x07u8;

        // Recode: new_piece = c1 * piece1 + c2 * piece2  (GF(2^8))
        let mut recoded_cv = vec![0u8; 4];
        let mut recoded_data = vec![0u8; 64];
        for i in 0..4 {
            recoded_cv[i] = gf256_mul(c1, cv1[i]) ^ gf256_mul(c2, cv2[i]);
        }
        for i in 0..64 {
            recoded_data[i] = gf256_mul(c1, data1[i]) ^ gf256_mul(c2, data2[i]);
        }

        // Method A: compute tag directly on the recoded piece.
        let tag_direct = compute_tag(&key, &recoded_cv, &recoded_data);

        // Method B: combine original tags algebraically.
        let tag_combined = combine_tags(&key, &[tag1, tag2], &[c1, c2]);

        assert_eq!(
            tag_direct, tag_combined,
            "HomMAC must be homomorphic: combine_tags == compute_tag on recoded piece"
        );
    }

    /// Verify the homomorphic property holds for 3 pieces with random data.
    #[test]
    fn combine_tags_homomorphic_three_pieces() {
        let key = HomMacKey::generate();

        let cv_len = 8;
        let data_len = 128;

        // Generate 3 random-ish pieces.
        let pieces: Vec<(Vec<u8>, Vec<u8>)> = (0..3)
            .map(|p| {
                let cv: Vec<u8> = (0..cv_len)
                    .map(|i| ((p * 37 + i * 13) % 251) as u8)
                    .collect();
                let data: Vec<u8> = (0..data_len)
                    .map(|i| ((p * 53 + i * 7) % 251) as u8)
                    .collect();
                (cv, data)
            })
            .collect();

        let tags: Vec<[u8; 32]> = pieces
            .iter()
            .map(|(cv, data)| compute_tag(&key, cv, data))
            .collect();

        // Random coefficients.
        let coeffs = [0x05u8, 0xAB, 0x3F];

        // Recode.
        let mut recoded_cv = vec![0u8; cv_len];
        let mut recoded_data = vec![0u8; data_len];
        for (p, (cv, data)) in pieces.iter().enumerate() {
            for i in 0..cv_len {
                recoded_cv[i] ^= gf256_mul(coeffs[p], cv[i]);
            }
            for i in 0..data_len {
                recoded_data[i] ^= gf256_mul(coeffs[p], data[i]);
            }
        }

        let tag_direct = compute_tag(&key, &recoded_cv, &recoded_data);
        let tag_combined = combine_tags(&key, &tags, &coeffs);

        assert_eq!(tag_direct, tag_combined);
    }
}
