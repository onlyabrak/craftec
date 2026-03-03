//! RLNC Decoder — recovers original data via Gaussian elimination over GF(2⁸).
//!
//! The [`RlncDecoder`] is a **client-side** component.  It collects incoming
//! coded pieces, maintaining an augmented matrix whose rows are the coding
//! vectors.  Partial Gaussian elimination is performed incrementally as each
//! piece arrives so that linear-dependency checks are O(k) rather than O(k²).
//! When the matrix reaches full rank `k`, back-substitution yields the original
//! source pieces.
//!
//! # Algorithm
//!
//! The decoder maintains an **augmented matrix** of size `k × (k + piece_size)`
//! where the left `k` columns hold the coding vectors and the right `piece_size`
//! columns hold the corresponding coded data.  Each new piece is row-reduced
//! using partial pivoting into row-echelon form (one row per pivot column).
//! Once `rank == k` the matrix is in full row-echelon form; back-substitution
//! converts it to reduced row-echelon form (identity on the left), yielding the
//! original source pieces on the right.
//!
//! # Thread safety
//!
//! [`RlncDecoder`] is **not** `Sync`; it holds mutable state.  Wrap in a
//! `Mutex` or use from a single task.

use tracing::{debug, info, trace, warn};

use craftec_types::piece::CodedPiece;

use crate::error::{Result, RlncError};
use crate::gf256::{gf_inv, gf_mul, gf_vec_mul_add};

// ── RlncDecoder ───────────────────────────────────────────────────────────────

/// Progressive RLNC decoder over GF(2⁸).
///
/// Collect at least `k` linearly-independent coded pieces by calling
/// [`add_piece`], then call [`decode`] to recover the original data.
pub struct RlncDecoder {
    /// Generation size.
    k: u32,
    /// Size of each source piece in bytes.
    piece_size: usize,
    /// Augmented matrix rows: each row is `[coding_vector | data]`.
    /// Stored in partial row-echelon form (one pivot per row, in column order).
    /// Length grows from 0 to k.
    matrix: Vec<Vec<u8>>,
    /// For each row, the column index of its leading pivot (1 per row, -1 if
    /// the row slot is empty).  `pivot_col[r] == c` means `matrix[r][c] == 1`
    /// after normalisation.
    pivot_col: Vec<Option<usize>>,
    /// Current matrix rank (number of linearly-independent pieces received).
    rank: u32,
    /// Whether [`decode`] has already been called successfully.
    decoded: bool,
}

impl RlncDecoder {
    /// Create a new, empty decoder for a generation of size `k`.
    ///
    /// # Arguments
    ///
    /// * `k`          — generation size (must match the encoder's `k`).
    /// * `piece_size` — byte size of each source piece.
    pub fn new(k: u32, piece_size: usize) -> Self {
        info!(k = k, piece_size = piece_size, "RLNC: decoder initialized");

        let k_usize = k as usize;
        Self {
            k,
            piece_size,
            // Pre-allocate k row slots; each slot is initially empty.
            matrix: vec![Vec::new(); k_usize],
            pivot_col: vec![None; k_usize],
            rank: 0,
            decoded: false,
        }
    }

    /// Return the generation size `k`.
    #[inline]
    pub fn k(&self) -> u32 {
        self.k
    }

    /// Return the current number of linearly-independent pieces received.
    #[inline]
    pub fn rank(&self) -> u32 {
        self.rank
    }

    /// Return `true` when enough independent pieces have been received to decode.
    #[inline]
    pub fn is_decodable(&self) -> bool {
        self.rank == self.k
    }

    /// Return progress as a value in `[0.0, 1.0]` where `1.0` means decodable.
    #[inline]
    pub fn progress(&self) -> f64 {
        self.rank as f64 / self.k as f64
    }

    /// Try to add a coded piece to the decoding matrix.
    ///
    /// Returns `Ok(true)` if the piece was linearly independent and increased
    /// the rank.  Returns `Ok(false)` if the piece was linearly dependent and
    /// was discarded.
    ///
    /// # Errors
    ///
    /// Returns [`RlncError::CodingVectorLengthMismatch`] if the piece's coding
    /// vector length differs from `k`.
    /// Returns [`RlncError::InvalidPieceSize`] if the piece's data length
    /// differs from `piece_size`.
    pub fn add_piece(&mut self, piece: &CodedPiece) -> Result<bool> {
        let k = self.k as usize;

        // Validate dimensions.
        if piece.coding_vector.len() != k {
            return Err(RlncError::CodingVectorLengthMismatch {
                expected: k,
                got: piece.coding_vector.len(),
            });
        }
        if piece.data.len() != self.piece_size {
            return Err(RlncError::InvalidPieceSize {
                expected: self.piece_size,
                got: piece.data.len(),
            });
        }

        // Build the augmented row: [coding_vector | data].
        let mut row = Vec::with_capacity(k + self.piece_size);
        row.extend_from_slice(&piece.coding_vector);
        row.extend_from_slice(&piece.data);

        // Partial Gaussian elimination: reduce this row against all existing
        // pivot rows to find where it belongs (or whether it is dependent).
        for r in 0..k {
            if let Some(col) = self.pivot_col[r] {
                let coeff = row[col];
                if coeff == 0 {
                    continue; // pivot column already zeroed — skip.
                }
                // row = row + coeff * pivot_row
                // (We work over GF(2⁸) so subtraction == addition == XOR)
                let pivot_row = self.matrix[r].clone();
                gf_vec_mul_add(&mut row, &pivot_row, coeff);
            }
        }

        // Find the first non-zero coefficient in the reduced row (new pivot).
        let pivot = row[..k].iter().position(|&b| b != 0);
        match pivot {
            None => {
                // The row is all zeros — linearly dependent.
                warn!(
                    rank = self.rank,
                    k = self.k,
                    "RLNC: discarded linearly dependent piece"
                );
                Ok(false)
            }
            Some(col) => {
                // Normalise so that the pivot coefficient is 1.
                let inv = gf_inv(row[col]);
                for b in row.iter_mut() {
                    *b = gf_mul(*b, inv);
                }

                // Store in the slot for this pivot column.
                self.matrix[col] = row;
                self.pivot_col[col] = Some(col);
                self.rank += 1;

                debug!(
                    rank = self.rank,
                    k = self.k,
                    pivot_col = col,
                    "RLNC: piece added, rank {}/{}",
                    self.rank,
                    self.k
                );

                Ok(true)
            }
        }
    }

    /// Decode the original data after collecting `k` independent pieces.
    ///
    /// Performs back-substitution to convert the row-echelon form to reduced
    /// row-echelon form (identity matrix on the left), then concatenates the
    /// right-hand side to reconstruct the source data.
    ///
    /// # Errors
    ///
    /// * [`RlncError::InsufficientPieces`] — fewer than `k` independent pieces.
    /// * [`RlncError::DecodeFailed`]       — matrix is unexpectedly singular.
    pub fn decode(&mut self) -> Result<Vec<u8>> {
        let k = self.k as usize;

        if self.rank < self.k {
            return Err(RlncError::InsufficientPieces {
                have: self.rank,
                need: self.k,
            });
        }

        // Sanity: every pivot slot must be filled.
        for col in 0..k {
            if self.pivot_col[col].is_none() || self.matrix[col].is_empty() {
                return Err(RlncError::DecodeFailed(format!(
                    "pivot slot {col} is empty despite rank == k"
                )));
            }
        }

        trace!("RLNC: starting back-substitution on {}×{} matrix", k, k);

        // Back-substitution: for each pivot row (from bottom to top), eliminate
        // its pivot coefficient from all rows above it.
        for col in (0..k).rev() {
            // Row for this pivot is self.matrix[col].
            for row in 0..col {
                let coeff = self.matrix[row][col];
                if coeff == 0 {
                    continue;
                }
                // Borrow-checker workaround: clone the pivot row.
                let pivot_row = self.matrix[col].clone();
                gf_vec_mul_add(&mut self.matrix[row], &pivot_row, coeff);
            }
        }

        // Extract the original source pieces from the right-hand side of each row.
        let mut data: Vec<u8> = Vec::with_capacity(k * self.piece_size);
        for col in 0..k {
            let row = &self.matrix[col];
            // Validate the left-hand side is identity for this row.
            if row[col] != 1 {
                return Err(RlncError::DecodeFailed(format!(
                    "row {col}: expected pivot 1, got {}",
                    row[col]
                )));
            }
            data.extend_from_slice(&row[k..]);
        }

        self.decoded = true;
        info!("RLNC: decode complete, {} bytes recovered", data.len());

        Ok(data)
    }

    /// Return whether this decoder has already decoded successfully.
    #[inline]
    pub fn is_decoded(&self) -> bool {
        self.decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::RlncEncoder;

    /// Encode `data` with generation size `k`, collect all coded pieces, decode.
    fn roundtrip(data: &[u8], k: u32) -> Vec<u8> {
        let encoder = RlncEncoder::new(data, k).expect("encoder");
        let n = encoder.target_pieces() as usize;
        let pieces = encoder.encode_n(n);

        let mut decoder = RlncDecoder::new(k, encoder.piece_size());
        for piece in &pieces {
            let _ = decoder.add_piece(piece);
            if decoder.is_decodable() {
                break;
            }
        }
        assert!(decoder.is_decodable(), "decoder did not reach full rank");
        decoder.decode().expect("decode failed")
    }

    #[test]
    fn roundtrip_small() {
        let data = b"Hello, RLNC world!";
        let recovered = roundtrip(data, 4);
        // The recovered data is padded; check prefix matches.
        assert_eq!(&recovered[..data.len()], data);
    }

    #[test]
    fn roundtrip_exact_block() {
        // 256 bytes, k=4 → piece_size=64, no padding needed.
        let data: Vec<u8> = (0u16..256).map(|i| i as u8).collect();
        let recovered = roundtrip(&data, 4);
        assert_eq!(&recovered[..256], data.as_slice());
    }

    #[test]
    fn roundtrip_k32() {
        // Standard generation: 8 KiB, k=32.
        let data: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
        let recovered = roundtrip(&data, 32);
        assert_eq!(&recovered[..8192], data.as_slice());
    }

    #[test]
    fn roundtrip_all_zeros() {
        let data = vec![0u8; 1024];
        let recovered = roundtrip(&data, 8);
        assert!(recovered.iter().all(|&b| b == 0));
    }

    #[test]
    fn roundtrip_all_ones() {
        let data = vec![0xFF_u8; 512];
        let recovered = roundtrip(&data, 4);
        assert_eq!(&recovered[..512], data.as_slice());
    }

    #[test]
    fn add_piece_rejects_wrong_coding_vector_length() {
        let mut dec = RlncDecoder::new(4, 64);
        let enc = RlncEncoder::new(&vec![0u8; 256], 8).unwrap(); // k=8, not 4
        let piece = enc.encode_piece();
        let result = dec.add_piece(&piece);
        assert!(matches!(
            result,
            Err(RlncError::CodingVectorLengthMismatch { .. })
        ));
    }

    #[test]
    fn add_piece_rejects_wrong_data_size() {
        use craftec_types::cid::Cid;

        let cid = Cid::from_data(b"test");
        let cv = vec![1u8; 4]; // k=4
        let data = vec![0u8; 32]; // wrong size (expect 64)
        let tag = [0u8; 32]; // tag irrelevant — piece rejected by size check
        let piece = CodedPiece::new(cid, cv, data, tag);

        let mut dec = RlncDecoder::new(4, 64);
        let result = dec.add_piece(&piece);
        assert!(matches!(result, Err(RlncError::InvalidPieceSize { .. })));
    }

    #[test]
    fn decode_requires_full_rank() {
        let encoder = RlncEncoder::new(&vec![1u8; 512], 8).unwrap();
        let mut decoder = RlncDecoder::new(8, encoder.piece_size());
        // Add only 4 pieces.
        let pieces = encoder.encode_n(4);
        for p in &pieces {
            let _ = decoder.add_piece(p);
        }
        let result = decoder.decode();
        assert!(matches!(result, Err(RlncError::InsufficientPieces { .. })));
    }

    #[test]
    fn dependent_piece_is_discarded() {
        let encoder = RlncEncoder::new(&vec![7u8; 64], 4).unwrap();
        let mut decoder = RlncDecoder::new(4, encoder.piece_size());
        // Add the same piece twice — second should be rejected.
        let piece = encoder.encode_piece();
        let r1 = decoder.add_piece(&piece).unwrap();
        let r2 = decoder.add_piece(&piece).unwrap();
        assert!(r1, "first piece should be accepted");
        assert!(!r2, "duplicate piece should be rejected as dependent");
    }

    #[test]
    fn progress_tracks_rank() {
        let encoder = RlncEncoder::new(&vec![5u8; 256], 4).unwrap();
        let mut decoder = RlncDecoder::new(4, encoder.piece_size());
        assert!((decoder.progress() - 0.0).abs() < f64::EPSILON);
        for _ in 0..4 {
            let p = encoder.encode_piece();
            let _ = decoder.add_piece(&p);
        }
        // After 4 pieces, should be decodable (progress == 1.0) given random
        // vectors are almost certainly independent.
        // (Probability of failure: < 4/(2^8) ≈ 1.5%)
        if decoder.is_decodable() {
            assert!((decoder.progress() - 1.0).abs() < f64::EPSILON);
        }
    }
}
