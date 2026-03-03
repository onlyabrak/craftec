//! # CraftOBJ Content-Addressed Store
//!
//! [`ContentAddressedStore`] is the central storage abstraction in CraftOBJ.
//! It provides a **content-addressed key-value store** where:
//!
//! - Keys are [`Cid`] values (BLAKE3 hashes of the stored bytes).
//! - Values are immutable byte buffers.
//! - The filesystem is the index — objects are stored as files named by their
//!   hex-encoded CID, sharded into 256 subdirectories.
//!
//! ## Performance layers
//!
//! Reads consult three layers in order:
//!
//! 1. **LRU cache** — microsecond-latency for hot objects.
//! 2. **Bloom filter** — eliminates disk I/O for absent CIDs with near-zero
//!    false-negative probability.
//! 3. **Filesystem** — the ground truth; always verified with BLAKE3.
//!
//! ## Integrity guarantee
//!
//! Every object read from disk is re-hashed with BLAKE3 and verified against
//! the requested CID. Silent corruption (bitrot, partial writes, filesystem
//! bugs) is **always detected** and reported as
//! [`ObjError::IntegrityViolation`](crate::error::ObjError::IntegrityViolation).
//!
//! ## Concurrency
//!
//! The store is `Clone + Send + Sync` via `Arc`-wrapped internals. Multiple
//! tasks/threads can call `get`, `put`, `contains`, and `delete` concurrently.
//! Object writes are atomic at the OS level: data is written to a `.tmp` file
//! first and then renamed into place, so a concurrent reader never observes a
//! partial write.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use craftec_types::Cid;
use parking_lot::RwLock;
use tracing::{debug, error, info, trace};

use crate::bloom::CidBloomFilter;
use crate::cache::ObjectCache;
use crate::error::{ObjError, Result};
use crate::shard;

// ─── Metrics ────────────────────────────────────────────────────────────────

/// Atomic counters exposed by [`ContentAddressedStore`] for observability.
///
/// All fields are `AtomicU64` so they can be read without taking any lock.
/// Metrics are updated on every store operation.
///
/// # Reading metrics
///
/// ```rust,ignore
/// let m = store.metrics();
/// let hits = m.cache_hits.load(Ordering::Relaxed);
/// ```
#[derive(Debug)]
pub struct StoreMetrics {
    /// Total calls to [`ContentAddressedStore::put`].
    pub puts: AtomicU64,
    /// Total calls to [`ContentAddressedStore::get`].
    pub gets: AtomicU64,
    /// Number of `get` calls satisfied from the LRU cache.
    pub cache_hits: AtomicU64,
    /// Number of `get` calls that required a disk read.
    pub cache_misses: AtomicU64,
    /// Number of bloom-positive responses that turned out to be false positives
    /// (i.e. the bloom said "maybe present" but the file was not on disk).
    pub bloom_false_positives: AtomicU64,
    /// Number of objects whose BLAKE3 hash did not match their CID on read.
    pub integrity_violations: AtomicU64,
}

impl Default for StoreMetrics {
    fn default() -> Self {
        StoreMetrics {
            puts: AtomicU64::new(0),
            gets: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            bloom_false_positives: AtomicU64::new(0),
            integrity_violations: AtomicU64::new(0),
        }
    }
}

// ─── Store internals ────────────────────────────────────────────────────────

/// Shared state behind an `Arc`. Split out so that `ContentAddressedStore` can
/// be cheaply cloned.
struct Inner {
    base_dir: PathBuf,
    cache: ObjectCache,
    bloom: RwLock<CidBloomFilter>,
    metrics: StoreMetrics,
    event_tx: RwLock<Option<tokio::sync::broadcast::Sender<craftec_types::Event>>>,
}

// ─── ContentAddressedStore ──────────────────────────────────────────────────

/// Content-addressed immutable object store backed by the local filesystem.
///
/// # Construction
///
/// ```rust,ignore
/// let store = ContentAddressedStore::new(Path::new("/var/craftobj"), 4096)?;
/// ```
///
/// The second argument is the LRU cache capacity in number of objects.
///
/// # Concurrency
///
/// The store is cheaply clonable — all clones share the same underlying state
/// through `Arc`. This makes it ergonomic to pass to multiple Tokio tasks.
///
/// # Atomic writes
///
/// Objects are written atomically: data is first written to a temporary file
/// (`<shard_path>.tmp`) and then renamed into place. Concurrent readers will
/// never observe a partial object.
#[derive(Clone)]
pub struct ContentAddressedStore {
    inner: Arc<Inner>,
}

impl ContentAddressedStore {
    /// Create (or re-open) a CraftOBJ store rooted at `base_dir`.
    ///
    /// On first call:
    /// - Creates all 256 shard subdirectories under `base_dir`.
    /// - Rebuilds the in-memory bloom filter from existing files.
    /// - Initialises the LRU cache with `cache_capacity` slots.
    ///
    /// On subsequent calls (restart):
    /// - Walks existing files to rebuild the bloom filter and count objects.
    /// - The LRU cache starts empty (cold start).
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or if an I/O error
    /// occurs while walking the shard directories.
    pub fn new(base_dir: &Path, cache_capacity: usize) -> Result<Self> {
        debug!(
            base_dir = ?base_dir,
            cache_capacity,
            "CraftOBJ store: initialising"
        );

        // Create the base directory and all 256 shards.
        std::fs::create_dir_all(base_dir).map_err(ObjError::IoError)?;
        shard::ensure_shard_dirs(base_dir)?;

        // Rebuild bloom filter from existing objects.
        let bloom = CidBloomFilter::rebuild(base_dir)?;
        let count = bloom.len();

        let inner = Arc::new(Inner {
            base_dir: base_dir.to_owned(),
            cache: ObjectCache::new(cache_capacity),
            bloom: RwLock::new(bloom),
            metrics: StoreMetrics::default(),
            event_tx: RwLock::new(None),
        });

        info!(
            base_dir = ?base_dir,
            existing_cids = count,
            "CraftOBJ store initialized at {:?}, {} existing CIDs",
            base_dir,
            count
        );

        Ok(ContentAddressedStore { inner })
    }

    /// Inject an event sender so the store can publish [`CidWritten`](craftec_types::Event::CidWritten)
    /// events after successful writes.
    ///
    /// Called after the event bus is initialised — the store is created before
    /// the bus, so this uses a post-init setter rather than constructor injection.
    pub fn set_event_sender(&self, tx: tokio::sync::broadcast::Sender<craftec_types::Event>) {
        *self.inner.event_tx.write() = Some(tx);
    }

    // ── Write ────────────────────────────────────────────────────────────────

    /// Store `data` and return its content identifier.
    ///
    /// The CID is computed by hashing `data` with BLAKE3. If an object with
    /// this CID is already present, the call is a no-op (content-addressed
    /// deduplication) and the existing CID is returned immediately.
    ///
    /// # Write atomicity
    ///
    /// The data is written to a `.tmp` file and then `rename`d into its final
    /// position. This guarantees that concurrent readers never observe a
    /// partially written object.
    ///
    /// # Errors
    ///
    /// - [`ObjError::StoreFull`] if the filesystem has no space.
    /// - [`ObjError::IoError`] on other I/O failures.
    pub async fn put(&self, data: &[u8]) -> Result<Cid> {
        let put_start = std::time::Instant::now();
        let cid = Cid::from_data(data);
        self.inner.metrics.puts.fetch_add(1, Ordering::Relaxed);

        debug!(
            cid = %cid,
            size = data.len(),
            "CraftOBJ: put object"
        );

        // Fast path: bloom filter + disk check for deduplication.
        if self.inner.bloom.read().probably_contains(&cid) {
            let path = shard::shard_path(&self.inner.base_dir, &cid);
            if tokio::fs::metadata(&path).await.is_ok() {
                trace!(cid = %cid, "CraftOBJ: put — object already exists, skipping write");
                return Ok(cid);
            }
        }

        // Write to a temporary file, then rename atomically.
        let dest = shard::shard_path(&self.inner.base_dir, &cid);
        let tmp = dest.with_extension("tmp");

        tokio::fs::write(&tmp, data).await.map_err(|e| {
            if e.raw_os_error() == Some(libc_enospc()) {
                ObjError::StoreFull
            } else {
                ObjError::IoError(e)
            }
        })?;

        tokio::fs::rename(&tmp, &dest).await.map_err(|e| {
            // Clean up the orphaned tmp file on rename failure.
            let _ = std::fs::remove_file(&tmp);
            ObjError::IoError(e)
        })?;

        // Update in-memory state.
        self.inner.bloom.write().insert(&cid);
        self.inner.cache.put(cid, Bytes::copy_from_slice(data));

        // Publish CidWritten event (only on actual writes, not dedup).
        if let Some(tx) = self.inner.event_tx.read().as_ref() {
            let _ = tx.send(craftec_types::Event::CidWritten { cid });
        }

        trace!(
            cid = %cid,
            size = data.len(),
            duration_ms = put_start.elapsed().as_millis() as u64,
            "CraftOBJ: put object — write complete"
        );
        Ok(cid)
    }

    // ── Read ─────────────────────────────────────────────────────────────────

    /// Retrieve an object by its CID.
    ///
    /// The read path is:
    /// 1. Check the LRU cache (zero disk I/O).
    /// 2. Check the bloom filter (skip disk if definitely absent).
    /// 3. Read from the shard filesystem path.
    /// 4. **Verify** `BLAKE3(bytes) == cid` — integrity is always checked.
    /// 5. Populate the LRU cache for future reads.
    ///
    /// # Integrity
    ///
    /// If the on-disk bytes do not match the CID, the method returns
    /// [`ObjError::IntegrityViolation`] and increments the
    /// `integrity_violations` metric. The caller should treat this as a
    /// critical fault and schedule repair from the network.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(bytes))` — object found and verified.
    /// - `Ok(None)` — object is not present in this store.
    /// - `Err(ObjError::IntegrityViolation)` — object found but corrupted.
    pub async fn get(&self, cid: &Cid) -> Result<Option<Bytes>> {
        let get_start = std::time::Instant::now();
        self.inner.metrics.gets.fetch_add(1, Ordering::Relaxed);

        // Layer 1: LRU cache.
        if let Some(bytes) = self.inner.cache.get(cid) {
            self.inner
                .metrics
                .cache_hits
                .fetch_add(1, Ordering::Relaxed);
            debug!(cid = %cid, cache_hit = true, "CraftOBJ: get object");
            return Ok(Some(bytes));
        }

        self.inner
            .metrics
            .cache_misses
            .fetch_add(1, Ordering::Relaxed);

        // Layer 2: Bloom filter.
        if !self.inner.bloom.read().probably_contains(cid) {
            debug!(cid = %cid, layer = "bloom", "CraftOBJ: get — bloom miss, returning None");
            return Ok(None);
        }

        // Layer 3: Disk.
        let path = shard::shard_path(&self.inner.base_dir, cid);
        let data = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Bloom false positive.
                self.inner
                    .metrics
                    .bloom_false_positives
                    .fetch_add(1, Ordering::Relaxed);
                trace!(cid = %cid, "CraftOBJ: get — bloom false positive");
                return Ok(None);
            }
            Err(e) => return Err(ObjError::IoError(e)),
        };

        // Layer 4: Integrity verification — CRITICAL.
        let actual_cid = Cid::from_data(&data);
        if actual_cid != *cid {
            self.inner
                .metrics
                .integrity_violations
                .fetch_add(1, Ordering::Relaxed);
            error!(
                cid = %cid,
                actual_cid = %actual_cid,
                path = ?path,
                "CraftOBJ: INTEGRITY VIOLATION — hash mismatch"
            );
            return Err(ObjError::IntegrityViolation {
                cid: cid.to_string(),
                msg: format!("stored bytes hash to {} but CID is {}", actual_cid, cid),
            });
        }

        let bytes = Bytes::from(data);

        // Populate LRU cache for subsequent reads.
        self.inner.cache.put(*cid, bytes.clone());

        debug!(
            cid = %cid,
            layer = "disk",
            duration_ms = get_start.elapsed().as_millis() as u64,
            "CraftOBJ: get object — disk read"
        );
        Ok(Some(bytes))
    }

    // ── Membership ───────────────────────────────────────────────────────────

    /// Check whether a CID is present in this store.
    ///
    /// Uses the bloom filter for a fast "definitely not here" check, then
    /// verifies with a filesystem `metadata` call. Does **not** read or verify
    /// the object bytes.
    pub async fn contains(&self, cid: &Cid) -> bool {
        trace!(cid = %cid, "CraftOBJ: contains check");

        if !self.inner.bloom.read().probably_contains(cid) {
            return false;
        }

        let path = shard::shard_path(&self.inner.base_dir, cid);
        tokio::fs::metadata(&path).await.is_ok()
    }

    // ── Delete ───────────────────────────────────────────────────────────────

    /// Remove an object from the store.
    ///
    /// Removes the file from disk, evicts it from the LRU cache. The bloom
    /// filter cannot remove individual entries, so after deletion the bloom may
    /// still return `true` for this CID until the filter is rebuilt. This is
    /// harmless — a subsequent `get` will detect the missing file and return
    /// `None`.
    ///
    /// # Returns
    ///
    /// - `Ok(true)` — the object existed and was deleted.
    /// - `Ok(false)` — the object was not present (no-op).
    pub async fn delete(&self, cid: &Cid) -> Result<bool> {
        debug!(cid = %cid, "CraftOBJ: delete object");

        let path = shard::shard_path(&self.inner.base_dir, cid);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                self.inner.cache.remove(cid);
                trace!(cid = %cid, "CraftOBJ: delete — removed from disk and cache");
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                trace!(cid = %cid, "CraftOBJ: delete — object was not present");
                Ok(false)
            }
            Err(e) => Err(ObjError::IoError(e)),
        }
    }

    // ── Enumeration ──────────────────────────────────────────────────────────

    /// List all CIDs stored in this store.
    ///
    /// Walks all 256 shard directories and returns the CIDs of every valid
    /// object file. This is an O(n) operation in the number of stored objects.
    ///
    /// # Errors
    ///
    /// Returns an error if any shard directory cannot be read.
    pub async fn list_cids(&self) -> Result<Vec<Cid>> {
        // Walk shards on the blocking thread pool to avoid blocking the async executor.
        let base_dir = self.inner.base_dir.clone();
        let cids = tokio::task::spawn_blocking(move || shard::walk_shards(&base_dir))
            .await
            .map_err(|e| {
                ObjError::IoError(std::io::Error::other(format!(
                    "spawn_blocking panicked: {}",
                    e
                )))
            })??;

        debug!(count = cids.len(), "CraftOBJ: listed all CIDs");
        Ok(cids)
    }

    // ── Accounting ───────────────────────────────────────────────────────────

    /// Compute the total bytes used by all stored objects.
    ///
    /// Sums the sizes of every file across all 256 shard directories. Does not
    /// include filesystem metadata overhead or directory entries.
    ///
    /// This is a synchronous operation; call it from a blocking thread or via
    /// `tokio::task::spawn_blocking` if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if any directory entry cannot be stat'd.
    pub fn disk_usage(&self) -> Result<u64> {
        let mut total: u64 = 0;
        for i in 0u8..=255 {
            let shard_dir = self.inner.base_dir.join(format!("{:02x}", i));
            if !shard_dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&shard_dir).map_err(ObjError::IoError)? {
                let entry = entry.map_err(ObjError::IoError)?;
                let meta = entry.metadata().map_err(ObjError::IoError)?;
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
        trace!(total_bytes = total, "CraftOBJ: disk_usage computed");
        Ok(total)
    }

    /// Count the number of objects currently stored on disk.
    ///
    /// Counts every regular file in all 256 shard directories.
    ///
    /// # Errors
    ///
    /// Returns an error if any shard directory cannot be read.
    pub fn object_count(&self) -> Result<usize> {
        let mut count: usize = 0;
        for i in 0u8..=255 {
            let shard_dir = self.inner.base_dir.join(format!("{:02x}", i));
            if !shard_dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&shard_dir).map_err(ObjError::IoError)? {
                let entry = entry.map_err(ObjError::IoError)?;
                if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
                    count += 1;
                }
            }
        }
        trace!(count, "CraftOBJ: object_count computed");
        Ok(count)
    }

    // ── Observability ────────────────────────────────────────────────────────

    /// Return a reference to the store's metric counters.
    ///
    /// All fields are `AtomicU64` and can be read with `Ordering::Relaxed`
    /// for monitoring purposes.
    pub fn metrics(&self) -> &StoreMetrics {
        &self.inner.metrics
    }

    /// Return the filesystem path of the store's base directory.
    pub fn base_dir(&self) -> &Path {
        &self.inner.base_dir
    }
}

// ─── Platform shim ──────────────────────────────────────────────────────────

/// Returns the errno value for "no space left on device" on the current
/// platform. Used to distinguish `ENOSPC` from other I/O errors.
#[inline]
fn libc_enospc() -> i32 {
    // ENOSPC is 28 on Linux, macOS, and most other Unix-likes.
    28
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Construct a test store in a temporary directory.
    async fn make_store() -> (ContentAddressedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ContentAddressedStore::new(dir.path(), 64).unwrap();
        (store, dir)
    }

    // ── put / get roundtrip ──────────────────────────────────────────────────

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (store, _dir) = make_store().await;
        let data = b"hello craftec world";
        let cid = store.put(data).await.unwrap();
        let retrieved = store
            .get(&cid)
            .await
            .unwrap()
            .expect("object must be found");
        assert_eq!(retrieved.as_ref(), data);
    }

    #[tokio::test]
    async fn put_returns_correct_cid() {
        let (store, _dir) = make_store().await;
        let data = b"content-addressed!";
        let cid = store.put(data).await.unwrap();
        // CID must equal BLAKE3(data).
        assert_eq!(cid, Cid::from_data(data));
    }

    #[tokio::test]
    async fn put_deduplication() {
        let (store, _dir) = make_store().await;
        let data = b"duplicate me";
        let cid1 = store.put(data).await.unwrap();
        let cid2 = store.put(data).await.unwrap();
        assert_eq!(cid1, cid2);
        assert_eq!(store.object_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn get_returns_none_for_absent_cid() {
        let (store, _dir) = make_store().await;
        let absent = Cid::from_data(b"not in store");
        let result = store.get(&absent).await.unwrap();
        assert!(result.is_none());
    }

    // ── Integrity verification ───────────────────────────────────────────────

    #[tokio::test]
    async fn integrity_violation_detected() {
        let (store, _dir) = make_store().await;
        let data = b"original content";
        let cid = store.put(data).await.unwrap();

        // Corrupt the file on disk.
        let path = shard::shard_path(store.base_dir(), &cid);
        tokio::fs::write(&path, b"corrupted!").await.unwrap();

        // Evict from cache so the disk path is taken.
        store.inner.cache.remove(&cid);

        let result = store.get(&cid).await;
        assert!(
            matches!(result, Err(ObjError::IntegrityViolation { .. })),
            "expected IntegrityViolation, got {:?}",
            result
        );
        assert_eq!(
            store.metrics().integrity_violations.load(Ordering::Relaxed),
            1
        );
    }

    // ── Bloom filter behaviour ───────────────────────────────────────────────

    #[tokio::test]
    async fn bloom_filter_eliminates_disk_io_for_absent() {
        let (store, _dir) = make_store().await;
        // Store is empty; bloom is empty; get must return None without disk I/O.
        let absent = Cid::from_data(b"definitely absent");
        let result = store.get(&absent).await.unwrap();
        assert!(result.is_none());
        // Cache misses == 1 because we counted the miss, but no bloom hit.
        assert_eq!(store.metrics().cache_misses.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn bloom_is_populated_on_put() {
        let (store, _dir) = make_store().await;
        let data = b"bloom me";
        let cid = store.put(data).await.unwrap();
        assert!(store.inner.bloom.read().probably_contains(&cid));
    }

    // ── LRU cache behaviour ──────────────────────────────────────────────────

    #[tokio::test]
    async fn second_get_is_cache_hit() {
        let (store, _dir) = make_store().await;
        let data = b"cache this";
        let cid = store.put(data).await.unwrap();

        // First get — cache cold after put (put also warms the cache).
        store.get(&cid).await.unwrap();
        let hits_before = store.metrics().cache_hits.load(Ordering::Relaxed);

        // Second get — must be a cache hit.
        store.get(&cid).await.unwrap();
        let hits_after = store.metrics().cache_hits.load(Ordering::Relaxed);
        assert!(hits_after > hits_before, "second get should be a cache hit");
    }

    #[tokio::test]
    async fn cache_eviction_falls_back_to_disk() {
        // Use a tiny cache so eviction happens quickly.
        let dir = tempfile::tempdir().unwrap();
        let store = ContentAddressedStore::new(dir.path(), 2).unwrap();

        // Put 5 objects into a cache that holds only 2.
        let mut cids: Vec<Cid> = Vec::new();
        for i in 0u8..5 {
            let cid = store.put(&[i; 16]).await.unwrap();
            cids.push(cid);
        }

        // The oldest cached items should have been evicted from the LRU cache;
        // disk should still serve them correctly.
        for cid in &cids {
            let result = store.get(cid).await.unwrap();
            assert!(
                result.is_some(),
                "evicted objects must still be readable from disk"
            );
        }
    }

    // ── contains ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn contains_true_after_put() {
        let (store, _dir) = make_store().await;
        let cid = store.put(b"check me").await.unwrap();
        assert!(store.contains(&cid).await);
    }

    #[tokio::test]
    async fn contains_false_for_absent() {
        let (store, _dir) = make_store().await;
        assert!(!store.contains(&Cid::from_data(b"nope")).await);
    }

    // ── delete ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_returns_true_on_success() {
        let (store, _dir) = make_store().await;
        let cid = store.put(b"deletable").await.unwrap();
        assert!(store.delete(&cid).await.unwrap());
    }

    #[tokio::test]
    async fn delete_returns_false_for_absent() {
        let (store, _dir) = make_store().await;
        let absent = Cid::from_data(b"absent");
        assert!(!store.delete(&absent).await.unwrap());
    }

    #[tokio::test]
    async fn get_after_delete_returns_none() {
        let (store, _dir) = make_store().await;
        let cid = store.put(b"delete then get").await.unwrap();
        store.delete(&cid).await.unwrap();
        // Also evict from cache by calling remove explicitly (delete does this).
        assert!(store.get(&cid).await.unwrap().is_none());
    }

    // ── list_cids ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_cids_empty_store() {
        let (store, _dir) = make_store().await;
        assert!(store.list_cids().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_cids_returns_all_objects() {
        let (store, _dir) = make_store().await;
        let payloads: &[&[u8]] = &[b"alpha", b"beta", b"gamma"];
        let mut expected: Vec<Cid> = Vec::new();
        for p in payloads {
            let cid = store.put(p).await.unwrap();
            expected.push(cid);
        }
        let mut found = store.list_cids().await.unwrap();
        found.sort();
        expected.sort();
        assert_eq!(found, expected);
    }

    // ── disk_usage / object_count ────────────────────────────────────────────

    #[tokio::test]
    async fn disk_usage_increases_on_put() {
        let (store, _dir) = make_store().await;
        let before = store.disk_usage().unwrap();
        store.put(b"some data that takes up space").await.unwrap();
        let after = store.disk_usage().unwrap();
        assert!(after > before);
    }

    #[tokio::test]
    async fn object_count_tracks_puts_and_deletes() {
        let (store, _dir) = make_store().await;
        assert_eq!(store.object_count().unwrap(), 0);

        let cid = store.put(b"count me").await.unwrap();
        assert_eq!(store.object_count().unwrap(), 1);

        store.delete(&cid).await.unwrap();
        assert_eq!(store.object_count().unwrap(), 0);
    }

    // ── Shard distribution ───────────────────────────────────────────────────

    #[tokio::test]
    async fn objects_go_into_correct_shard_dir() {
        let (store, _dir) = make_store().await;
        let data = b"shard routing test";
        let cid = store.put(data).await.unwrap();
        let hex = cid.to_string();
        let expected_shard = &hex[..2];
        let shard_path = shard::shard_path(store.base_dir(), &cid);
        assert!(shard_path.exists(), "object file must exist at shard path");
        let actual_shard = shard_path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(actual_shard, expected_shard);
    }

    // ── Metrics ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn metrics_puts_incremented() {
        let (store, _dir) = make_store().await;
        store.put(b"m1").await.unwrap();
        store.put(b"m2").await.unwrap();
        assert_eq!(store.metrics().puts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn metrics_gets_incremented() {
        let (store, _dir) = make_store().await;
        let cid = store.put(b"metric get").await.unwrap();
        store.get(&cid).await.unwrap();
        store.get(&cid).await.unwrap();
        assert_eq!(store.metrics().gets.load(Ordering::Relaxed), 2);
    }

    // ── Persistence across restart ───────────────────────────────────────────

    #[tokio::test]
    async fn data_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"persisted!";
        let cid;
        {
            let store = ContentAddressedStore::new(dir.path(), 64).unwrap();
            cid = store.put(data).await.unwrap();
        }
        // Re-open the store in the same directory.
        let store2 = ContentAddressedStore::new(dir.path(), 64).unwrap();
        let result = store2.get(&cid).await.unwrap();
        assert_eq!(result.as_deref(), Some(data.as_ref()));
    }

    // ── Clone shares state ───────────────────────────────────────────────────

    #[tokio::test]
    async fn clone_shares_state() {
        let (store, _dir) = make_store().await;
        let clone = store.clone();

        let cid = store.put(b"shared").await.unwrap();
        // The clone should see the same object.
        assert!(clone.contains(&cid).await);
    }

    // ── Event publishing ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn put_publishes_cid_written_event() {
        let (store, _dir) = make_store().await;
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        store.set_event_sender(tx);

        let cid = store.put(b"event-test-data").await.unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            craftec_types::Event::CidWritten { cid: ecid } => assert_eq!(ecid, cid),
            other => panic!("expected CidWritten, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn put_dedup_does_not_publish() {
        let (store, _dir) = make_store().await;
        let data = b"dedup-test";

        // First put (no event sender yet).
        store.put(data).await.unwrap();

        // Now attach sender.
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        store.set_event_sender(tx);

        // Second put should dedup — no event.
        store.put(data).await.unwrap();

        // Channel should be empty.
        assert!(
            rx.try_recv().is_err(),
            "dedup path should not publish event"
        );
    }
}
