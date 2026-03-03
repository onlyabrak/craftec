//! # Bloom filter for CID membership testing
//!
//! [`CidBloomFilter`] provides a space-efficient probabilistic membership test
//! that lets the [`ContentAddressedStore`](crate::store::ContentAddressedStore)
//! rule out disk I/O for CIDs that are **definitely not** present.
//!
//! ## Design rationale
//!
//! A bloom filter sits in front of every `get` and `contains` call. On a
//! cache miss it is consulted before touching the filesystem:
//!
//! - **Definitely not present** (`probably_contains` → `false`): skip disk I/O
//!   entirely, return `None` immediately.
//! - **Possibly present** (`probably_contains` → `true`): read from disk and
//!   verify the CID — false positives are handled by the subsequent disk check.
//!
//! The false-positive rate is configured at construction time. A rate of
//! `0.01` (1 %) means roughly 1 in 100 bloom-positive responses require a
//! disk check that will find nothing.
//!
//! ## Rebuild on startup
//!
//! Because the bloom filter is in-memory only, [`CidBloomFilter::rebuild`]
//! walks all shard directories on startup and re-inserts every resident CID.
//! This is O(n) in the number of stored objects but requires no serialisation
//! format for the filter state itself.

use std::path::Path;

use bloomfilter::Bloom;
use craftec_types::Cid;
use tracing::{debug, trace};

use crate::error::Result;
use crate::shard;

/// Default expected item count used when constructing a fresh filter.
const DEFAULT_EXPECTED_ITEMS: usize = 1_000_000;

/// Default target false-positive rate (1 %).
const DEFAULT_FP_RATE: f64 = 0.01;

/// A bloom filter keyed on [`Cid`] values.
///
/// All insertions and queries operate on the raw 32-byte CID representation.
/// The underlying [`bloomfilter::Bloom`] handles the hash functions internally.
///
/// # Thread safety
///
/// [`CidBloomFilter`] is **not** `Sync` on its own. The
/// [`ContentAddressedStore`](crate::store::ContentAddressedStore) wraps it in a
/// `parking_lot::RwLock` so that concurrent readers and exclusive writers work
/// correctly.
pub struct CidBloomFilter {
    inner: Bloom<[u8; 32]>,
    /// Running count of insertions (for logging / metrics).
    count: usize,
}

impl CidBloomFilter {
    /// Create a new, empty bloom filter.
    ///
    /// # Arguments
    ///
    /// * `expected_items` – estimated maximum number of distinct CIDs that will
    ///   be inserted. Under-estimating inflates the false-positive rate.
    /// * `fp_rate` – target false-positive probability in `[0, 1)`. A value of
    ///   `0.01` means 1 % false positives at `expected_items` capacity.
    ///
    /// # Panics
    ///
    /// Panics if `fp_rate` is not in `(0.0, 1.0)` (propagated from the
    /// underlying `bloomfilter` crate).
    pub fn new(expected_items: usize, fp_rate: f64) -> Self {
        trace!(
            expected_items,
            fp_rate, "CraftOBJ bloom: constructing new filter"
        );
        let inner = Bloom::new_for_fp_rate(expected_items, fp_rate);
        CidBloomFilter { inner, count: 0 }
    }

    /// Insert a CID into the filter.
    ///
    /// After this call, [`probably_contains`](Self::probably_contains) will
    /// return `true` for `cid`.  Insertions are idempotent with respect to
    /// membership queries (though the internal bit-set is still set again).
    pub fn insert(&mut self, cid: &Cid) {
        trace!(cid = %cid, "CraftOBJ bloom: insert");
        self.inner.set(cid.as_bytes());
        self.count += 1;
    }

    /// Test whether `cid` **might** be present.
    ///
    /// Returns:
    /// - `false` — the CID is **definitely not** in the store (zero false
    ///   negatives by the bloom filter guarantee).
    /// - `true` — the CID **may** be in the store; a disk check is required to
    ///   confirm (false positives possible at the configured rate).
    pub fn probably_contains(&self, cid: &Cid) -> bool {
        let result = self.inner.check(cid.as_bytes());
        trace!(cid = %cid, result, "CraftOBJ bloom: probably_contains");
        result
    }

    /// Number of [`insert`](Self::insert) calls made since construction or last
    /// rebuild.
    ///
    /// This is a monotonically increasing counter and does not deduplicate
    /// re-insertions of the same CID.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns `true` if no items have been inserted.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Rebuild the bloom filter by walking all shard directories under
    /// `base_dir` and inserting every CID found on disk.
    ///
    /// Called once during [`ContentAddressedStore`](crate::store::ContentAddressedStore)
    /// initialisation to restore the in-memory filter after a restart.
    ///
    /// The filter is sized with [`DEFAULT_EXPECTED_ITEMS`] and
    /// [`DEFAULT_FP_RATE`]. If the store has grown beyond
    /// `DEFAULT_EXPECTED_ITEMS` objects the false-positive rate will be higher
    /// than intended but correctness is not affected.
    ///
    /// # Errors
    ///
    /// Returns an error if the shard directories cannot be walked (e.g. the
    /// base directory does not exist or a permission error occurs).
    pub fn rebuild(base_dir: &Path) -> Result<Self> {
        debug!(
            base_dir = ?base_dir,
            "CraftOBJ bloom: rebuilding from disk"
        );
        let mut filter = Self::new(DEFAULT_EXPECTED_ITEMS, DEFAULT_FP_RATE);
        let cids = shard::walk_shards(base_dir)?;
        let n = cids.len();
        for cid in cids {
            filter.insert(&cid);
        }
        debug!(
            base_dir = ?base_dir,
            inserted = n,
            "CraftOBJ bloom: rebuild complete"
        );
        Ok(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cid(data: &[u8]) -> Cid {
        Cid::from_data(data)
    }

    #[test]
    fn insert_then_probably_contains() {
        let mut f = CidBloomFilter::new(100, 0.01);
        let cid = make_cid(b"hello");
        assert!(
            !f.probably_contains(&cid),
            "empty filter should return false"
        );
        f.insert(&cid);
        assert!(f.probably_contains(&cid), "inserted CID must be found");
    }

    #[test]
    fn no_false_negatives() {
        let mut f = CidBloomFilter::new(1000, 0.001);
        let cids: Vec<Cid> = (0u32..500).map(|i| make_cid(&i.to_le_bytes())).collect();
        for c in &cids {
            f.insert(c);
        }
        for c in &cids {
            assert!(
                f.probably_contains(c),
                "bloom filter must never produce false negatives"
            );
        }
    }

    #[test]
    fn len_tracks_insertions() {
        let mut f = CidBloomFilter::new(100, 0.01);
        assert_eq!(f.len(), 0);
        assert!(f.is_empty());
        f.insert(&make_cid(b"a"));
        f.insert(&make_cid(b"b"));
        assert_eq!(f.len(), 2);
        assert!(!f.is_empty());
    }

    #[test]
    fn rebuild_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Create shard subdirectories so walk_shards doesn't error.
        crate::shard::ensure_shard_dirs(dir.path()).unwrap();
        let filter = CidBloomFilter::rebuild(dir.path()).unwrap();
        assert_eq!(filter.len(), 0);
        assert!(filter.is_empty());
    }

    #[test]
    fn rebuild_with_files() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        crate::shard::ensure_shard_dirs(dir.path()).unwrap();

        // Plant two fake object files.
        let data_a = b"object alpha";
        let data_b = b"object beta";
        let cid_a = Cid::from_data(data_a);
        let cid_b = Cid::from_data(data_b);

        for (cid, data) in [(&cid_a, data_a.as_ref()), (&cid_b, data_b.as_ref())] {
            let path = crate::shard::shard_path(dir.path(), cid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(data).unwrap();
        }

        let filter = CidBloomFilter::rebuild(dir.path()).unwrap();
        assert!(filter.probably_contains(&cid_a));
        assert!(filter.probably_contains(&cid_b));
    }
}
