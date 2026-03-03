//! Piece types for RLNC (Random Linear Network Coding) erasure coding.
//!
//! Craftec splits content into *pages* of [`PAGE_SIZE`] bytes and encodes them
//! into *coded pieces* using GF(2^8) arithmetic.  Each [`CodedPiece`] carries
//! a coding vector describing the linear combination and a homomorphic MAC tag
//! for integrity verification during recoding.

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::cid::Cid;

// ── Constants ──────────────────────────────────────────────────────────────

/// Default RLNC generation size — the number of source blocks per generation.
pub const K_DEFAULT: u32 = 32;

/// Page (piece) size in bytes — 16 KiB.
pub const PAGE_SIZE: usize = 16_384;

/// Galois field order used for RLNC arithmetic.
pub const GF_ORDER: u32 = 256;

// ── HomMAC ────────────────────────────────────────────────────────────────

/// A 32-byte homomorphic MAC tag for recoding integrity verification.
pub type HomMAC = [u8; 32];

// ── PieceId ────────────────────────────────────────────────────────────────

/// The identifier of a single coded piece — the BLAKE3 hash of its raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PieceId([u8; 32]);

impl PieceId {
    /// Compute a [`PieceId`] by hashing `data` with BLAKE3.
    pub fn from_data(data: &[u8]) -> Self {
        trace!(data_len = data.len(), "computing PieceId from data");
        let hash = blake3::hash(data);
        let id = Self(*hash.as_bytes());
        debug!(piece_id = ?id, "computed PieceId");
        id
    }

    /// Return the raw byte digest.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for PieceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

// ── CodedPiece ─────────────────────────────────────────────────────────────

/// A single RLNC coded piece ready for network transmission or storage.
///
/// The `coding_vector` describes the GF(2^8) linear combination applied to the
/// original source blocks.  The `hommac_tag` is a homomorphic MAC computed
/// over the coding vector and data, enabling integrity checks during recoding
/// without re-downloading from the original source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodedPiece {
    /// Identifier of this specific coded piece (BLAKE3 of piece bytes).
    pub piece_id: PieceId,

    /// The CID of the original content this piece belongs to.
    pub cid: Cid,

    /// GF(2^8) coding vector of length `k` (one coefficient per source block).
    pub coding_vector: Vec<u8>,

    /// Coded data payload.
    pub data: Vec<u8>,

    /// 32-byte homomorphic MAC tag for recoding integrity verification.
    pub hommac_tag: [u8; 32],
}

impl CodedPiece {
    /// Construct a new [`CodedPiece`].
    ///
    /// The `piece_id` is computed automatically from the piece bytes.
    pub fn new(cid: Cid, coding_vector: Vec<u8>, data: Vec<u8>, hommac_tag: [u8; 32]) -> Self {
        trace!(
            cid = %cid,
            coding_vector_len = coding_vector.len(),
            data_len = data.len(),
            "constructing CodedPiece"
        );
        let mut piece_bytes = Vec::with_capacity(coding_vector.len() + data.len());
        piece_bytes.extend_from_slice(&coding_vector);
        piece_bytes.extend_from_slice(&data);
        let piece_id = PieceId::from_data(&piece_bytes);
        debug!(piece_id = ?piece_id, cid = %cid, "created CodedPiece");
        Self {
            piece_id,
            cid,
            coding_vector,
            data,
            hommac_tag,
        }
    }

    /// Verify that the `piece_id` matches the BLAKE3 hash of `[coding_vector || data]`.
    pub fn verify_piece_id(&self) -> bool {
        let mut piece_bytes = Vec::with_capacity(self.coding_vector.len() + self.data.len());
        piece_bytes.extend_from_slice(&self.coding_vector);
        piece_bytes.extend_from_slice(&self.data);
        let expected = PieceId::from_data(&piece_bytes);
        expected == self.piece_id
    }

    /// Verify the homomorphic MAC tag using the CID as key.
    ///
    /// Recomputes the linear GF(2^8) inner-product MAC:
    /// `tag[j] = Σ_i r_{j,i} * piece[i]` where piece = cv || data
    /// and `r_j = BLAKE3_XOF(key || j)`.
    ///
    /// This matches `craftec_crypto::hommac::compute_tag`.
    pub fn verify_mac(&self) -> bool {
        let key = self.cid.as_bytes();
        let mut tag = [0u8; 32];

        for j in 0..32u8 {
            let mut hasher = blake3::Hasher::new();
            hasher.update(key);
            hasher.update(&[j]);
            let mut xof = hasher.finalize_xof();

            let mut acc = 0u8;
            let mut r_buf = [0u8; 256];

            // Process coding_vector.
            for cv_chunk in self.coding_vector.chunks(256) {
                xof.fill(&mut r_buf[..cv_chunk.len()]);
                for (r, cv) in r_buf.iter().zip(cv_chunk.iter()) {
                    acc ^= gf256_mul_0x11b(*r, *cv);
                }
            }

            // Process data.
            for data_chunk in self.data.chunks(256) {
                xof.fill(&mut r_buf[..data_chunk.len()]);
                for (r, d) in r_buf.iter().zip(data_chunk.iter()) {
                    acc ^= gf256_mul_0x11b(*r, *d);
                }
            }

            tag[j as usize] = acc;
        }

        tag == self.hommac_tag
    }
}

// ── GF(2^8) helper ────────────────────────────────────────────────────────

/// GF(2^8) multiply using AES polynomial 0x11B.
/// Duplicated here to avoid a circular dependency on craftec-crypto.
#[inline]
fn gf256_mul_0x11b(a: u8, b: u8) -> u8 {
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
            aa ^= 0x1B;
        }
        bb >>= 1;
    }
    result
}

// ── PieceIndex ─────────────────────────────────────────────────────────────

/// Lightweight index record stored once per CID describing piece layout.
///
/// This is the metadata needed to reconstruct content: how many pieces exist,
/// what the original file size was, and which CID they belong to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PieceIndex {
    /// CID of the content whose pieces this index describes.
    pub cid: Cid,

    /// Total number of coded pieces stored for this CID.
    pub piece_count: u32,

    /// Original content size in bytes before erasure coding.
    pub original_size: u64,
}

impl PieceIndex {
    /// Create a new [`PieceIndex`].
    pub fn new(cid: Cid, piece_count: u32, original_size: u64) -> Self {
        debug!(
            cid = %cid,
            piece_count,
            original_size,
            "created PieceIndex"
        );
        Self {
            cid,
            piece_count,
            original_size,
        }
    }
}

// ── Utilities ──────────────────────────────────────────────────────────────

/// Compute the redundancy factor for a given generation size `k`.
///
/// Returns the target ratio of stored pieces to original pieces.  A minimum
/// of `2.0` is maintained; for small `k` additional overhead compensates for
/// the higher decoding failure probability of small generations.
///
/// ```
/// # use craftec_types::piece::redundancy;
/// assert!((redundancy(32) - 2.5).abs() < 1e-9);
/// assert!((redundancy(16) - 3.0).abs() < 1e-9);
/// ```
pub fn redundancy(k: u32) -> f64 {
    trace!(k, "computing redundancy factor");
    let r = 2.0 + 16.0 / k as f64;
    debug!(k, redundancy = r, "computed redundancy factor");
    r
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redundancy_k32() {
        let r = redundancy(K_DEFAULT);
        assert!((r - 2.5).abs() < 1e-9, "expected 2.5 for k=32, got {r}");
    }

    #[test]
    fn redundancy_decreases_with_larger_k() {
        assert!(redundancy(64) < redundancy(32));
        assert!(redundancy(128) < redundancy(64));
    }

    #[test]
    fn coded_piece_round_trip_serde() {
        let cid = Cid::from_data(b"test content");
        let cv = vec![1u8, 0, 0, 0];
        let data = vec![0xABu8; PAGE_SIZE];
        let tag = [0u8; 32];
        let piece = CodedPiece::new(cid, cv.clone(), data.clone(), tag);
        let encoded = postcard::to_allocvec(&piece).unwrap();
        let decoded: CodedPiece = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.cid, cid);
        assert_eq!(decoded.coding_vector, cv);
        assert_eq!(decoded.data, data);
    }

    #[test]
    fn piece_id_deterministic() {
        let id1 = PieceId::from_data(b"piece data");
        let id2 = PieceId::from_data(b"piece data");
        assert_eq!(id1, id2);
    }
}
