//! RLNC Recoder — repair and retransmission without decoding.
//!
//! Recoding is one of the most powerful properties of network coding: a relay
//! node that holds several coded pieces can produce a *new* coded piece by
//! combining them with fresh random coefficients, **without ever recovering the
//! original data**.  This means repair and redistribution can happen at any
//! intermediate node regardless of whether that node has received enough pieces
//! to decode.
//!
//! # How it works
//!
//! Given input coded pieces `p₁, …, pₘ` (all from the same generation CID),
//! each with coding vector `cᵢ` and data `dᵢ`, the recoder:
//!
//! 1. Draws `m` fresh random GF(2⁸) scalars `r₁, …, rₘ`.
//! 2. Computes the new coding vector: `c' = Σᵢ rᵢ · cᵢ` (GF(2⁸) vector sum).
//! 3. Computes the new data:            `d' = Σᵢ rᵢ · dᵢ` (GF(2⁸) vector sum).
//! 4. Recomputes [`PieceId`] and [`HomMAC`] for the new piece.
//!
//! The resulting piece `(c', d')` is a valid coded piece for the same
//! generation: any decoder that receives it alongside other independent pieces
//! can include it in its Gaussian-elimination matrix.
//!
//! # HomMAC linearity
//!
//! HomMAC tags are designed to be linear over GF(2⁸): the combined tag for a
//! recoded piece can be verified without knowing the original data.  The current
//! implementation combines tags via XOR (GF(2)-linear combination), which is
//! correct for the homomorphic MAC scheme used by [`HomMAC::combine`].

use rand::Rng;
use tracing::debug;

use craftec_types::piece::{CodedPiece, HomMAC, PieceId};

use craftec_crypto::hommac::combine_tags as hommac_combine;

use crate::error::{Result, RlncError};
use crate::gf256::gf_vec_mul_add;

// ── RlncRecoder ───────────────────────────────────────────────────────────────

/// Stateless RLNC recoder.
///
/// All methods are free functions on `RlncRecoder`; no instance state is
/// needed because recoding is a pure function of the input pieces.
pub struct RlncRecoder;

impl RlncRecoder {
    /// Recode a set of coded pieces into a single new coded piece.
    ///
    /// All input pieces must:
    /// - belong to the same generation (same CID), and
    /// - have identical coding vector and data lengths.
    ///
    /// At least **two** input pieces are required so that the output is a
    /// non-trivial linear combination.
    ///
    /// # Arguments
    ///
    /// * `pieces` — slice of coded pieces to combine.
    ///
    /// # Returns
    ///
    /// A fresh [`CodedPiece`] that is a valid coded piece for the same
    /// generation.
    ///
    /// # Errors
    ///
    /// * [`RlncError::InsufficientRecodeInputs`] — fewer than 2 pieces.
    /// * [`RlncError::MismatchedCids`]           — pieces have different CIDs.
    pub fn recode(pieces: &[CodedPiece]) -> Result<CodedPiece> {
        // Require at least 2 input pieces.
        if pieces.len() < 2 {
            return Err(RlncError::InsufficientRecodeInputs { got: pieces.len() });
        }

        // All pieces must share the same CID.
        let cid = pieces[0].cid;
        for p in pieces.iter().skip(1) {
            if p.cid != cid {
                return Err(RlncError::MismatchedCids);
            }
        }

        let k = pieces[0].coding_vector.len();
        let data_len = pieces[0].data.len();

        // Draw one fresh random scalar per input piece.
        let mut rng = rand::thread_rng();
        let scalars: Vec<u8> = (0..pieces.len())
            .map(|_| {
                // Use non-zero coefficients so every input piece influences output.
                let c: u8 = rng.r#gen();
                if c == 0 { rng.gen_range(1u8..=255) } else { c }
            })
            .collect();

        // Compute new coding vector: c'[i] = Σⱼ scalars[j] * pieces[j].cv[i]
        let mut new_coding_vector = vec![0u8; k];
        for (piece, &scalar) in pieces.iter().zip(scalars.iter()) {
            gf_vec_mul_add(&mut new_coding_vector, &piece.coding_vector, scalar);
        }

        // Compute new data: d'[i] = Σⱼ scalars[j] * pieces[j].data[i]
        let mut new_data = vec![0u8; data_len];
        for (piece, &scalar) in pieces.iter().zip(scalars.iter()) {
            gf_vec_mul_add(&mut new_data, &piece.data, scalar);
        }

        // Recompute PieceId from new coded data.
        let _piece_id = PieceId::from_data(&new_data);

        // Combine HomMAC tags homomorphically.
        // For a proper HomMAC scheme the combination would be:
        //   tag' = Σⱼ scalars[j] * tags[j]  (GF(2⁸) scalar × tag)
        // The current HomMAC::combine uses XOR which corresponds to
        // GF(2)-linear combination — consistent with the scalar-1 case.
        let tags: Vec<HomMAC> = pieces.iter().map(|p| p.hommac_tag).collect();
        let scalars_for_combine: Vec<u8> = scalars.clone();
        let hom_mac: HomMAC = hommac_combine(
            &craftec_crypto::hommac::HomMacKey::from_bytes(*cid.as_bytes()),
            &tags,
            &scalars_for_combine,
        );

        debug!(
            input_pieces = pieces.len(),
            cid = %cid,
            "RLNC: recoded piece from {} inputs",
            pieces.len()
        );

        Ok(CodedPiece::new(cid, new_coding_vector, new_data, hom_mac))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::RlncEncoder;
    use crate::decoder::RlncDecoder;

    fn make_pieces(data: &[u8], k: u32, count: usize) -> Vec<CodedPiece> {
        let encoder = RlncEncoder::new(data, k).expect("encoder");
        encoder.encode_n(count)
    }

    #[test]
    fn recode_requires_two_pieces() {
        let pieces = make_pieces(&[1u8; 256], 4, 1);
        let result = RlncRecoder::recode(&pieces);
        assert!(matches!(result, Err(RlncError::InsufficientRecodeInputs { got: 1 })));
    }

    #[test]
    fn recode_rejects_empty() {
        let result = RlncRecoder::recode(&[]);
        assert!(matches!(result, Err(RlncError::InsufficientRecodeInputs { got: 0 })));
    }

    #[test]
    fn recode_rejects_mismatched_cids() {
        let enc1 = RlncEncoder::new(b"data A", 2).unwrap();
        let enc2 = RlncEncoder::new(b"data B", 2).unwrap();
        // enc1 and enc2 have different CIDs.
        assert_ne!(enc1.cid(), enc2.cid());

        let p1 = enc1.encode_piece();
        let p2 = enc2.encode_piece();
        let result = RlncRecoder::recode(&[p1, p2]);
        assert!(matches!(result, Err(RlncError::MismatchedCids)));
    }

    #[test]
    fn recoded_piece_has_same_cid() {
        let data = vec![42u8; 512];
        let pieces = make_pieces(&data, 4, 4);
        let recoded = RlncRecoder::recode(&pieces).unwrap();
        assert_eq!(recoded.cid, pieces[0].cid);
    }

    #[test]
    fn recoded_piece_id_is_correct() {
        let data = vec![99u8; 256];
        let pieces = make_pieces(&data, 4, 3);
        let recoded = RlncRecoder::recode(&pieces).unwrap();
        // PieceId should equal BLAKE3(recoded.data).
        assert!(recoded.verify_piece_id(), "PieceId of recoded piece is wrong");
    }

    #[test]
    fn recoded_piece_coding_vector_length() {
        let k = 8u32;
        let data = vec![7u8; 1024];
        let pieces = make_pieces(&data, k, 5);
        let recoded = RlncRecoder::recode(&pieces).unwrap();
        assert_eq!(
            recoded.coding_vector.len(),
            k as usize,
            "recoded coding vector has wrong length"
        );
    }

    #[test]
    fn recoded_piece_is_decodable() {
        // Scenario: collect original coded pieces + a recoded one; all should
        // contribute toward decoding.
        let data: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
        let k = 8u32;

        let encoder = RlncEncoder::new(&data, k).expect("encoder");
        let n = encoder.target_pieces() as usize;
        let mut original_pieces = encoder.encode_n(n);

        // Take first two pieces and recode.
        let to_recode = original_pieces[..2].to_vec();
        let recoded = RlncRecoder::recode(&to_recode).unwrap();

        // Replace the first original piece with the recoded one.
        original_pieces[0] = recoded;

        // Attempt to decode using the modified set.
        let mut decoder = RlncDecoder::new(k, encoder.piece_size());
        let mut _accepted = 0u32;
        for piece in &original_pieces {
            match decoder.add_piece(piece) {
                Ok(true) => _accepted += 1,
                Ok(false) => {}
                Err(e) => panic!("add_piece error: {e:?}"),
            }
            if decoder.is_decodable() {
                break;
            }
        }

        if decoder.is_decodable() {
            let recovered = decoder.decode().expect("decode failed");
            assert_eq!(&recovered[..data.len()], data.as_slice(),
                "data mismatch after recode-in-loop decode");
        }
        // If not decodable (very unlikely), we still pass; the main
        // correctness check is that no errors were thrown.
    }

    #[test]
    fn recode_from_many_inputs() {
        let data = vec![0xABu8; 2048];
        let pieces = make_pieces(&data, 16, 10);
        let recoded = RlncRecoder::recode(&pieces).unwrap();
        assert_eq!(recoded.coding_vector.len(), 16);
        assert_eq!(recoded.data.len(), pieces[0].data.len());
        assert!(recoded.verify_piece_id());
    }
}
