//! RLNC Encoder — produces random linear combinations of source pieces.
//!
//! The [`RlncEncoder`] takes a blob of original data, splits it into `K`
//! equally-sized source pieces, and can produce an unlimited stream of
//! *coded pieces*, each of which is a random linear combination over GF(2⁸).
//!
//! # Rateless encoding
//!
//! Because coding coefficients are chosen uniformly at random from GF(2⁸)ˢ,
//! the encoder is *rateless*: it can produce as many coded pieces as desired
//! without pre-allocating a fixed rate. The default redundancy formula
//! `redundancy(k) = 2.0 + 16.0 / k` ensures the target piece count yields
//! enough overhead to make decoding failure probability negligible.
//!
//! # Thread safety
//!
//! [`RlncEncoder`] is `Send + Sync`. The hot-path method [`encode_piece`]
//! takes a shared reference and allocates per-call.

use rand::Rng;
use tracing::{debug, info};

use craftec_types::cid::Cid;
use craftec_types::piece::{CodedPiece, HomMAC, PieceId};

use craftec_crypto::hommac::compute_tag as hommac_compute;

use crate::error::{Result, RlncError};
use crate::gf256::gf_vec_mul_add;

// ── Redundancy formula ────────────────────────────────────────────────────────

/// Compute the target redundancy ratio for a generation of size `k`.
///
/// `redundancy(k) = 2.0 + 16.0 / k`
///
/// At `k = 32` this gives ~2.5×.  At `k = 8` it gives 4.0×.
#[inline]
pub fn redundancy(k: u32) -> f64 {
    2.0 + 16.0 / k as f64
}

/// Compute the target number of coded pieces for a generation of size `k`.
///
/// `target = ceil(k * redundancy(k))`
#[inline]
pub fn target_n(k: u32) -> u32 {
    let n = k as f64 * redundancy(k);
    n.ceil() as u32
}

// ── RlncEncoder ──────────────────────────────────────────────────────────────

/// RLNC encoder over GF(2⁸).
///
/// Splits original data into `K` source pieces and generates random linear
/// combinations on demand.
///
/// # Example
///
/// ```rust
/// use craftec_rlnc::encoder::RlncEncoder;
///
/// let data = vec![0u8; 8 * 1024]; // 8 KiB
/// let encoder = RlncEncoder::new(&data, 8).unwrap();
/// let pieces = encoder.encode_n(encoder.target_pieces() as usize);
/// assert_eq!(pieces.len() as u32, encoder.target_pieces());
/// ```
pub struct RlncEncoder {
    /// Generation size — number of source pieces.
    k: u32,
    /// Size of each source piece in bytes (original pieces are padded to this).
    piece_size: usize,
    /// The `K` source pieces derived from the original data.
    original_pieces: Vec<Vec<u8>>,
    /// Content identifier of the original data blob.
    cid: Cid,
}

impl RlncEncoder {
    /// Create a new [`RlncEncoder`] for the given data.
    ///
    /// The data is split into exactly `k` source pieces. If `data.len()` is
    /// not divisible by `k` the last piece is zero-padded.
    ///
    /// # Errors
    ///
    /// Returns [`RlncError::InsufficientPieces`] if `k == 0`.
    pub fn new(data: &[u8], k: u32) -> Result<Self> {
        if k == 0 {
            return Err(RlncError::InsufficientPieces { have: 0, need: 1 });
        }

        let total = data.len();
        // Piece size: ceiling division so all data fits in K pieces.
        let piece_size = if total == 0 {
            1
        } else {
            (total + k as usize - 1) / k as usize
        };

        // Split data into K pieces, padding the last one with zeros.
        let mut original_pieces: Vec<Vec<u8>> = Vec::with_capacity(k as usize);
        for i in 0..k as usize {
            let start = i * piece_size;
            let end = ((i + 1) * piece_size).min(total);
            let mut piece = vec![0u8; piece_size];
            if start < total {
                piece[..end - start].copy_from_slice(&data[start..end]);
            }
            original_pieces.push(piece);
        }

        // Compute the CID of the original data blob.
        let cid = Cid::from_data(data);

        info!(
            cid = %cid,
            k = k,
            piece_size = piece_size,
            total_bytes = total,
            "RLNC: encoder initialized"
        );

        Ok(Self {
            k,
            piece_size,
            original_pieces,
            cid,
        })
    }

    /// Return the generation size `K`.
    #[inline]
    pub fn k(&self) -> u32 {
        self.k
    }

    /// Return the piece size in bytes.
    #[inline]
    pub fn piece_size(&self) -> usize {
        self.piece_size
    }

    /// Return the CID of the original data.
    #[inline]
    pub fn cid(&self) -> &Cid {
        &self.cid
    }

    /// Compute the target number of coded pieces for this generation.
    ///
    /// `target_pieces = ceil(K * redundancy(K))`
    #[inline]
    pub fn target_pieces(&self) -> u32 {
        target_n(self.k)
    }

    /// Generate a single coded piece with a fresh random coding vector.
    ///
    /// Each call draws `K` random GF(2⁸) coefficients uniformly, computes
    /// the linear combination of source pieces, and attaches a [`HomMAC`] tag.
    ///
    /// This method is cheap enough to call in a tight loop.
    pub fn encode_piece(&self) -> CodedPiece {
        let mut rng = rand::thread_rng();

        // Draw K random coefficients from GF(2⁸) \ {0} to ensure non-trivial
        // combinations. (A zero coefficient would simply drop a source piece
        // from the combination — statistically unlikely to matter, but we want
        // dense random matrices for fastest decoding convergence.)
        let coding_vector: Vec<u8> = (0..self.k)
            .map(|_| {
                let c: u8 = rng.r#gen();
                // Re-roll zeros to keep the coding vector dense.
                if c == 0 { rng.gen_range(1..=255) } else { c }
            })
            .collect();

        // Compute coded_data[i] = XOR over j of (coding_vector[j] * piece_j[i])
        let mut coded_data = vec![0u8; self.piece_size];
        for (j, piece) in self.original_pieces.iter().enumerate() {
            gf_vec_mul_add(&mut coded_data, piece, coding_vector[j]);
        }

        // PieceId = BLAKE3(coded_data)
        let piece_id = PieceId::from_data(&coded_data);

        // Compute HomMAC tag.
        let hom_mac: HomMAC = hommac_compute(
            &craftec_crypto::hommac::HomMacKey::from_bytes(*self.cid.as_bytes()),
            &coding_vector,
            &coded_data,
        );

        debug!(
            piece_id = %piece_id,
            cid = %self.cid,
            "RLNC: encoded piece"
        );

        CodedPiece::new(self.cid, coding_vector, coded_data, hom_mac)
    }

    /// Generate exactly `n` coded pieces.
    ///
    /// Uses [`encode_piece`] internally; each piece has an independent random
    /// coding vector.
    ///
    /// # Arguments
    ///
    /// * `n` — number of coded pieces to produce.
    pub fn encode_n(&self, n: usize) -> Vec<CodedPiece> {
        info!(
            n = n,
            k = self.k,
            "RLNC: batch encoding {} pieces",
            n
        );
        (0..n).map(|_| self.encode_piece()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: create an encoder over `size` bytes with generation size `k`.
    fn make_encoder(size: usize, k: u32) -> RlncEncoder {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        RlncEncoder::new(&data, k).expect("encoder construction failed")
    }

    #[test]
    fn construction_splits_correctly() {
        let enc = make_encoder(1024, 4);
        assert_eq!(enc.k(), 4);
        assert_eq!(enc.original_pieces.len(), 4);
        assert_eq!(enc.piece_size(), 256);
    }

    #[test]
    fn padding_fills_zeros() {
        // 7 bytes, k=3 → piece_size = ceil(7/3) = 3
        let data = vec![1u8, 2, 3, 4, 5, 6, 7];
        let enc = RlncEncoder::new(&data, 3).unwrap();
        assert_eq!(enc.piece_size(), 3);
        // Last piece contains [7, 0, 0] due to zero-padding.
        assert_eq!(enc.original_pieces[2], vec![7u8, 0, 0]);
    }

    #[test]
    fn cid_is_deterministic() {
        let data = vec![42u8; 512];
        let enc1 = RlncEncoder::new(&data, 8).unwrap();
        let enc2 = RlncEncoder::new(&data, 8).unwrap();
        assert_eq!(enc1.cid(), enc2.cid());
    }

    #[test]
    fn encode_piece_has_correct_metadata() {
        let enc = make_encoder(4096, 8);
        let piece = enc.encode_piece();
        assert_eq!(piece.cid, enc.cid);
        assert_eq!(piece.coding_vector.len(), enc.k() as usize);
        assert_eq!(piece.data.len(), enc.piece_size());
        assert!(piece.verify_piece_id(), "piece_id should match BLAKE3(data)");
        assert!(piece.verify_mac(), "HomMAC tag should verify");
    }

    #[test]
    fn encode_n_produces_correct_count() {
        let enc = make_encoder(2048, 4);
        let n = enc.target_pieces() as usize;
        let pieces = enc.encode_n(n);
        assert_eq!(pieces.len(), n);
    }

    #[test]
    fn target_pieces_formula() {
        // redundancy(32) = 2.0 + 16/32 = 2.5, target = ceil(32 * 2.5) = 80
        let enc = make_encoder(32 * 256, 32);
        assert_eq!(enc.target_pieces(), 80);
    }

    #[test]
    fn coding_vectors_are_dense() {
        // Re-rolling zeros means all coefficients should be non-zero.
        // (Probabilistically guaranteed; this test could in theory be flaky
        //  if the PRNG output is adversarial, but in practice it never is.)
        let enc = make_encoder(1024, 16);
        for _ in 0..10 {
            let piece = enc.encode_piece();
            assert!(
                piece.coding_vector.iter().all(|&c| c != 0),
                "coding vector contained a zero coefficient"
            );
        }
    }

    #[test]
    fn empty_data_encodes() {
        let enc = RlncEncoder::new(&[], 4).unwrap();
        let piece = enc.encode_piece();
        assert_eq!(piece.data.len(), enc.piece_size());
    }

    #[test]
    fn zero_k_returns_error() {
        let result = RlncEncoder::new(&[1, 2, 3], 0);
        assert!(result.is_err());
    }

    #[test]
    fn redundancy_values() {
        // redundancy(32) = 2.5
        assert!((redundancy(32) - 2.5).abs() < 1e-10);
        // redundancy(8) = 4.0
        assert!((redundancy(8) - 4.0).abs() < 1e-10);
        // redundancy(16) = 3.0
        assert!((redundancy(16) - 3.0).abs() < 1e-10);
    }
}
