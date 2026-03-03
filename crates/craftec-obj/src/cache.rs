//! # LRU object cache
//!
//! [`ObjectCache`] is a thread-safe LRU cache that stores recently-read object
//! bytes keyed by [`Cid`]. It sits in front of the filesystem in the
//! [`ContentAddressedStore`](crate::store::ContentAddressedStore) read path,
//! eliminating redundant disk I/O for hot objects.
//!
//! ## Thread safety
//!
//! The cache is wrapped in a `parking_lot::RwLock`:
//! - Multiple concurrent readers can call [`get`](ObjectCache::get)
//!   simultaneously.
//! - Writers (insert/remove) take an exclusive lock momentarily.
//!
//! Eviction is handled transparently by the underlying [`lru::LruCache`] when
//! the capacity is reached.
//!
//! ## Memory footprint
//!
//! Each cached entry stores a `Bytes` handle — a cheap reference-counted slice
//! into a shared allocation. The total memory used is approximately
//! `capacity × average_object_size`. Choose `capacity` based on available RAM
//! and your expected working-set size.

use bytes::Bytes;
use craftec_types::Cid;
use lru::LruCache;
use parking_lot::RwLock;
use std::num::NonZeroUsize;
use tracing::{debug, trace};

/// A thread-safe LRU cache of recently-accessed objects.
///
/// Internally wraps a `parking_lot::RwLock<LruCache<Cid, Bytes>>`.
///
/// # Example
///
/// ```rust,ignore
/// let cache = ObjectCache::new(512);
/// cache.put(cid, bytes.clone());
/// let hit = cache.get(&cid);
/// assert!(hit.is_some());
/// ```
pub struct ObjectCache {
    inner: RwLock<LruCache<Cid, Bytes>>,
    capacity: usize,
}

impl ObjectCache {
    /// Construct a new cache that can hold at most `capacity` objects.
    ///
    /// When the cache is full and a new object is inserted, the
    /// least-recently-used entry is evicted automatically.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("cache capacity must be > 0");
        debug!(capacity, "CraftOBJ cache: initialised LRU cache");
        ObjectCache {
            inner: RwLock::new(LruCache::new(cap)),
            capacity,
        }
    }

    /// Look up an object by CID.
    ///
    /// Returns `Some(bytes)` on a cache hit, `None` on a miss.
    ///
    /// Note: this method takes a **write** lock internally because
    /// `LruCache::get` updates the recency order (promoting the accessed
    /// entry to MRU position). Despite the mutable access, callers observe
    /// only read semantics.
    pub fn get(&self, cid: &Cid) -> Option<Bytes> {
        let mut guard = self.inner.write();
        match guard.get(cid).cloned() {
            Some(bytes) => {
                trace!(cid = %cid, size = bytes.len(), "CraftOBJ cache: HIT");
                Some(bytes)
            }
            None => {
                trace!(cid = %cid, "CraftOBJ cache: MISS");
                None
            }
        }
    }

    /// Peek at an object without updating its recency position.
    ///
    /// Useful for diagnostics when you want to check presence without
    /// side effects on the LRU order.
    pub fn peek(&self, cid: &Cid) -> Option<Bytes> {
        let guard = self.inner.read();
        guard.peek(cid).cloned()
    }

    /// Insert or replace an object in the cache.
    ///
    /// If the cache is at capacity, the LRU entry is evicted first.
    pub fn put(&self, cid: Cid, data: Bytes) {
        trace!(cid = %cid, size = data.len(), "CraftOBJ cache: put");
        let mut guard = self.inner.write();
        guard.put(cid, data);
    }

    /// Remove an object from the cache.
    ///
    /// Returns the removed bytes if the entry existed, `None` otherwise.
    pub fn remove(&self, cid: &Cid) -> Option<Bytes> {
        trace!(cid = %cid, "CraftOBJ cache: remove");
        let mut guard = self.inner.write();
        guard.pop(cid)
    }

    /// Current number of entries in the cache.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Maximum number of entries this cache can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Return `true` if the cache contains an entry for `cid` (without
    /// updating recency).
    pub fn contains(&self, cid: &Cid) -> bool {
        self.inner.read().contains(cid)
    }

    /// Evict all entries.
    pub fn clear(&self) {
        debug!("CraftOBJ cache: cleared");
        self.inner.write().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(tag: &[u8]) -> Cid {
        Cid::from_data(tag)
    }

    fn bytes(data: &[u8]) -> Bytes {
        Bytes::copy_from_slice(data)
    }

    #[test]
    fn basic_put_get() {
        let cache = ObjectCache::new(10);
        let c = cid(b"test");
        let b = bytes(b"hello");

        assert!(cache.get(&c).is_none());
        cache.put(c, b.clone());
        assert_eq!(cache.get(&c), Some(b));
    }

    #[test]
    fn cache_miss_returns_none() {
        let cache = ObjectCache::new(10);
        assert!(cache.get(&cid(b"absent")).is_none());
    }

    #[test]
    fn remove_evicts_entry() {
        let cache = ObjectCache::new(10);
        let c = cid(b"evict me");
        cache.put(c, bytes(b"data"));
        assert!(cache.get(&c).is_some());
        cache.remove(&c);
        assert!(cache.get(&c).is_none());
    }

    #[test]
    fn capacity_evicts_lru() {
        let cache = ObjectCache::new(3);

        // Fill to capacity.
        let c0 = cid(b"first");
        let c1 = cid(b"second");
        let c2 = cid(b"third");
        cache.put(c0, bytes(b"0"));
        cache.put(c1, bytes(b"1"));
        cache.put(c2, bytes(b"2"));

        // Access c0 to make it recently used.
        cache.get(&c0);

        // Insert a fourth entry — c1 should be evicted (LRU is c1 since c0
        // was refreshed and c2 was last inserted).
        let c3 = cid(b"fourth");
        cache.put(c3, bytes(b"3"));

        assert_eq!(cache.len(), 3);
        // c0 was accessed, c2 and c3 were inserted — c1 is LRU.
        assert!(cache.get(&c1).is_none(), "LRU entry should have been evicted");
        assert!(cache.get(&c0).is_some());
        assert!(cache.get(&c2).is_some());
        assert!(cache.get(&c3).is_some());
    }

    #[test]
    fn len_and_is_empty() {
        let cache = ObjectCache::new(5);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        cache.put(cid(b"a"), bytes(b"a"));
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn capacity_accessor() {
        let cache = ObjectCache::new(42);
        assert_eq!(cache.capacity(), 42);
    }

    #[test]
    fn contains_without_recency_update() {
        let cache = ObjectCache::new(2);
        let c = cid(b"check");
        assert!(!cache.contains(&c));
        cache.put(c, bytes(b"value"));
        assert!(cache.contains(&c));
    }

    #[test]
    fn clear_empties_cache() {
        let cache = ObjectCache::new(5);
        cache.put(cid(b"x"), bytes(b"x"));
        cache.put(cid(b"y"), bytes(b"y"));
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn overwrite_updates_value() {
        let cache = ObjectCache::new(5);
        let c = cid(b"overwrite");
        cache.put(c, bytes(b"old"));
        cache.put(c, bytes(b"new"));
        assert_eq!(cache.get(&c), Some(bytes(b"new")));
    }

    #[test]
    fn concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ObjectCache::new(100));
        let c = cid(b"shared");
        cache.put(c, bytes(b"concurrent"));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    assert!(cache.get(&c).is_some());
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}
