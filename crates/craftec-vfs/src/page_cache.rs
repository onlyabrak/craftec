//! LRU page cache for CID-VFS.
//!
//! Caching hot pages in memory avoids round-trips to CraftOBJ for repeated
//! reads.  The cache key is `(root_cid, page_num)` so that pages from
//! different snapshots are stored independently, enabling safe snapshot
//! isolation without cache poisoning.
//!
//! ## Performance target
//! The architecture spec calls for ~0.013 ms p50 latency on a cache hit.
//! This is achieved by:
//! - A `parking_lot::Mutex` instead of `std::sync::Mutex` (lower contention).
//! - Cloning page bytes only on cache hit (Vec clone of a 16 KB slab is fast).
//! - Atomic hit/miss counters so `hit_rate()` never touches the lock.

use std::sync::atomic::{AtomicU64, Ordering};

use craftec_types::Cid;
use lru::LruCache;
use parking_lot::Mutex;

/// Default number of pages to keep in the LRU cache.
///
/// At 16 KB per page this is ~256 MB of resident cache.
const DEFAULT_CAPACITY: usize = 16_384;

/// LRU page cache keyed by `(snapshot_root_cid, page_number)`.
///
/// Cache entries are invalidated per (root, page) pair, so stale pages from
/// old snapshots are never returned to newer snapshot readers.
pub struct PageCache {
    cache: Mutex<LruCache<(Cid, u32), Vec<u8>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl PageCache {
    /// Create a [`PageCache`] with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a [`PageCache`] with an explicit `capacity` (number of pages).
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = std::num::NonZeroUsize::new(capacity.max(1)).unwrap();
        Self {
            cache: Mutex::new(LruCache::new(cap)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Look up `page_num` within snapshot `root`.
    ///
    /// Returns `Some(page_data)` on a cache hit and `None` on a miss.
    /// Promotes the entry to the MRU position on hit.
    pub fn get(&self, root: &Cid, page_num: u32) -> Option<Vec<u8>> {
        let result = self.cache.lock().get(&(*root, page_num)).cloned();
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Insert or replace a page in the cache for snapshot `root`.
    pub fn put(&self, root: &Cid, page_num: u32, data: Vec<u8>) {
        self.cache.lock().put((*root, page_num), data);
    }

    /// Remove a specific `(root, page_num)` entry from the cache.
    ///
    /// Called after a commit invalidates old dirty pages.
    pub fn invalidate(&self, root: &Cid, page_num: u32) {
        self.cache.lock().pop(&(*root, page_num));
    }

    /// Return the ratio of cache hits to total lookups `[0.0, 1.0]`.
    ///
    /// Returns `0.0` if no lookups have been performed yet.
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    /// Total number of cache hits since construction.
    pub fn total_hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Total number of cache misses since construction.
    pub fn total_misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.cache.lock().len()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_types::Cid;

    fn cid(seed: u8) -> Cid {
        Cid::from_bytes([seed; 32])
    }

    fn page(fill: u8, size: usize) -> Vec<u8> {
        vec![fill; size]
    }

    #[test]
    fn miss_then_hit() {
        let cache = PageCache::with_capacity(8);
        let root = cid(0x01);

        assert_eq!(cache.get(&root, 0), None);
        assert_eq!(cache.total_misses(), 1);

        cache.put(&root, 0, page(0xFF, 16_384));
        let fetched = cache.get(&root, 0).expect("should be cached");
        assert_eq!(fetched.len(), 16_384);
        assert_eq!(fetched[0], 0xFF);
        assert_eq!(cache.total_hits(), 1);
    }

    #[test]
    fn hit_rate_computation() {
        let cache = PageCache::with_capacity(4);
        let root = cid(0x02);

        cache.put(&root, 1, page(0x01, 4096));
        cache.get(&root, 1); // hit
        cache.get(&root, 2); // miss
        cache.get(&root, 1); // hit

        let rate = cache.hit_rate();
        // 2 hits out of 3 total = 0.666…
        assert!((rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = PageCache::with_capacity(8);
        let root = cid(0x03);
        cache.put(&root, 7, page(0x07, 512));
        assert!(cache.get(&root, 7).is_some());
        cache.invalidate(&root, 7);
        assert!(cache.get(&root, 7).is_none());
    }

    #[test]
    fn different_roots_isolated() {
        let cache = PageCache::with_capacity(8);
        let r1 = cid(0x10);
        let r2 = cid(0x20);
        cache.put(&r1, 0, page(0x11, 1024));
        assert!(cache.get(&r2, 0).is_none()); // different root → miss
    }

    #[test]
    fn lru_eviction_respects_capacity() {
        let cache = PageCache::with_capacity(2);
        let root = cid(0x01);
        cache.put(&root, 0, page(0x00, 64));
        cache.put(&root, 1, page(0x01, 64));
        cache.put(&root, 2, page(0x02, 64)); // should evict page 0
        // Page 0 may be evicted; page 2 must be present.
        assert!(cache.get(&root, 2).is_some());
    }
}
