//! BLAKE3 hashing utilities.
//!
//! All content-addressed identifiers in Craftec are BLAKE3 digests.  This
//! module provides helpers for hashing raw bytes, hashing 16 KiB database
//! pages, verifying CIDs, and computing a binary Merkle tree root over a set
//! of leaf hashes.

use craftec_types::cid::Cid;
use tracing::{debug, trace};

/// Hash arbitrary bytes with BLAKE3 and return the raw 32-byte digest.
///
/// ```
/// # use craftec_crypto::hash::hash_bytes;
/// let h = hash_bytes(b"craftec");
/// assert_eq!(h.len(), 32);
/// ```
pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    trace!(data_len = data.len(), "hashing bytes with BLAKE3");
    let digest = *blake3::hash(data).as_bytes();
    debug!(
        data_len = data.len(),
        hash = hex::encode(digest),
        "computed BLAKE3 hash"
    );
    digest
}

/// Hash a database page (up to 16 KiB) and return its [`Cid`].
///
/// This is the canonical way to derive a piece CID from raw page bytes.
///
/// ```
/// # use craftec_crypto::hash::hash_page;
/// let page = vec![0u8; 16_384];
/// let cid = hash_page(&page);
/// assert_eq!(cid, hash_page(&page)); // deterministic
/// ```
pub fn hash_page(page_data: &[u8]) -> Cid {
    trace!(page_len = page_data.len(), "hashing page to CID");
    let cid = Cid::from_data(page_data);
    debug!(page_len = page_data.len(), cid = %cid, "hashed page to CID");
    cid
}

/// Verify that `data` hashes to `expected`.
///
/// Returns `true` if the content is authentic.
pub fn verify_cid(data: &[u8], expected: &Cid) -> bool {
    trace!(
        data_len = data.len(),
        expected_cid = %expected,
        "verifying CID against data"
    );
    let ok = expected.verify(data);
    debug!(expected_cid = %expected, verified = ok, "CID verification result");
    ok
}

/// Compute a binary Merkle tree root over `leaves`.
///
/// Each leaf is a 32-byte hash.  If `leaves` is empty, the zero hash is
/// returned.  Odd-length layers duplicate the last node (Bitcoin-style).
///
/// The internal node hash is computed as:
/// ```text
/// BLAKE3(left_child || right_child)
/// ```
pub fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    trace!(leaf_count = leaves.len(), "computing Merkle root");
    if leaves.is_empty() {
        debug!("merkle_root called with empty leaves, returning zero hash");
        return [0u8; 32];
    }

    let mut current: Vec<[u8; 32]> = leaves.to_vec();

    while current.len() > 1 {
        let mut next = Vec::with_capacity((current.len() + 1) / 2);
        let mut i = 0;
        while i < current.len() {
            let left = current[i];
            // Duplicate the last node if the layer has an odd number of nodes.
            let right = if i + 1 < current.len() {
                current[i + 1]
            } else {
                current[i]
            };
            let mut combined = [0u8; 64];
            combined[..32].copy_from_slice(&left);
            combined[32..].copy_from_slice(&right);
            next.push(*blake3::hash(&combined).as_bytes());
            i += 2;
        }
        current = next;
    }

    let root = current[0];
    debug!(
        leaf_count = leaves.len(),
        root = hex::encode(root),
        "computed Merkle root"
    );
    root
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_deterministic() {
        let h1 = hash_bytes(b"hello");
        let h2 = hash_bytes(b"hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_bytes_differs_for_different_input() {
        let h1 = hash_bytes(b"foo");
        let h2 = hash_bytes(b"bar");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_page_returns_cid() {
        let page = vec![42u8; 16_384];
        let cid = hash_page(&page);
        assert!(verify_cid(&page, &cid));
    }

    #[test]
    fn verify_cid_wrong_data() {
        let page = vec![1u8; 16_384];
        let cid = hash_page(&page);
        let tampered = vec![2u8; 16_384];
        assert!(!verify_cid(&tampered, &cid));
    }

    #[test]
    fn merkle_root_empty() {
        let root = merkle_root(&[]);
        assert_eq!(root, [0u8; 32]);
    }

    #[test]
    fn merkle_root_single_leaf() {
        let leaf = hash_bytes(b"only leaf");
        let root = merkle_root(&[leaf]);
        assert_eq!(root, leaf);
    }

    #[test]
    fn merkle_root_two_leaves_deterministic() {
        let a = hash_bytes(b"a");
        let b = hash_bytes(b"b");
        let r1 = merkle_root(&[a, b]);
        let r2 = merkle_root(&[a, b]);
        assert_eq!(r1, r2);
        assert_ne!(r1, merkle_root(&[b, a])); // order matters
    }

    #[test]
    fn merkle_root_odd_number_of_leaves() {
        let leaves: Vec<[u8; 32]> = (0..5u8).map(|i| hash_bytes(&[i])).collect();
        // Should not panic with an odd number of leaves.
        let root = merkle_root(&leaves);
        assert_ne!(root, [0u8; 32]);
    }
}
