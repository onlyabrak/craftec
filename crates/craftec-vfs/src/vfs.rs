//! CID-VFS: content-addressed virtual file system for SQLite.
//!
//! [`CidVfs`] is the core of the CID-VFS layer.  It maps SQLite page
//! read/write operations to CraftOBJ content-addressed storage via BLAKE3
//! content identifiers.
//!
//! ## Write path (per-transaction)
//! 1. SQLite calls the VFS `xWrite` callback → [`write_page`](CidVfs::write_page).
//! 2. Pages are buffered in `dirty_pages` (in-memory) until the transaction
//!    requests a sync.
//! 3. On [`commit`](CidVfs::commit):
//!    a. Each dirty page is stored in CraftOBJ via `store.put(page_bytes)`.
//!       CraftOBJ computes `CID = blake3(page_bytes)` internally.
//!    b. The page index is updated: `page_num → CID`.
//!    c. The full page index is serialised; its bytes are put into CraftOBJ
//!       to produce the new root CID.
//!    d. `dirty_pages` is cleared.
//!
//! ## Read path
//! 1. Check [`PageCache`] with `(current_root, page_num)` key.
//! 2. On cache miss: resolve `page_num → CID` from [`PageIndex`].
//! 3. Fetch page bytes from CraftOBJ via `store.get(cid)`.
//! 4. Verify BLAKE3 integrity against the stored CID.
//! 5. Populate cache and return page bytes.
//!
//! ## Snapshot isolation
//! A [`Snapshot`] pins the root CID at query start.  All reads within the
//! snapshot use the pinned entry map, immune to concurrent commits.
//!
//! ## No WAL
//! CraftOBJ is append-only: old pages are never overwritten.  Eviction of
//! unreferenced pages is handled by a future GC agent.

use std::collections::HashMap;
use std::sync::Arc;

use craftec_types::Cid;
use craftec_obj::ContentAddressedStore;
use parking_lot::{Mutex, RwLock};

use crate::error::{Result, VfsError};
use crate::page_cache::PageCache;
use crate::page_index::PageIndex;
use crate::snapshot::Snapshot;

/// Default SQLite page size used by CID-VFS: 16 KiB.
pub const DEFAULT_PAGE_SIZE: usize = 16_384;

/// CID-VFS: maps SQLite page operations to content-addressed storage.
///
/// Each 16 KiB SQLite page is BLAKE3-hashed (by CraftOBJ) to produce a CID,
/// then stored in CraftOBJ.  The page index (page_number → CID) is itself
/// serialised and stored to produce a *root CID* that uniquely identifies the
/// complete database state at any point in time.
///
/// ## Async API
/// All I/O methods (`read_page`, `commit`) are `async` because CraftOBJ
/// performs filesystem I/O.  `write_page` is synchronous — it only buffers
/// in-memory.
pub struct CidVfs {
    /// The underlying content-addressed store (CraftOBJ).
    store: Arc<ContentAddressedStore>,
    /// Live page-number → CID mapping.
    page_index: Arc<PageIndex>,
    /// Hot page cache.
    page_cache: Arc<PageCache>,
    /// Pages written during the current transaction but not yet committed.
    dirty_pages: Mutex<HashMap<u32, Vec<u8>>>,
    /// Root CID of the most recently committed database state.
    current_root: RwLock<Option<Cid>>,
    /// SQLite page size in bytes (must be a power of two, default 16 KiB).
    page_size: usize,
}

impl CidVfs {
    /// Construct a new [`CidVfs`] instance.
    ///
    /// # Arguments
    /// * `store` — the CraftOBJ content-addressed store to use.
    /// * `page_size` — SQLite page size in bytes.  Pass
    ///   [`DEFAULT_PAGE_SIZE`] for the standard 16 KiB layout.
    ///
    /// # Errors
    /// Returns [`VfsError::InvalidPageSize`] if `page_size` is not a
    /// power of two or is outside the range 512 – 65536.
    pub fn new(store: Arc<ContentAddressedStore>, page_size: usize) -> Result<Self> {
        if page_size < 512 || page_size > 65536 || !page_size.is_power_of_two() {
            return Err(VfsError::InvalidPageSize(page_size));
        }
        tracing::info!(page_size = page_size, "CID-VFS: initialized");
        Ok(Self {
            store,
            page_index: Arc::new(PageIndex::new()),
            page_cache: Arc::new(PageCache::new()),
            dirty_pages: Mutex::new(HashMap::new()),
            current_root: RwLock::new(None),
            page_size,
        })
    }

    /// Construct a [`CidVfs`] with the default 16 KiB page size.
    pub fn with_default_page_size(store: Arc<ContentAddressedStore>) -> Result<Self> {
        Self::new(store, DEFAULT_PAGE_SIZE)
    }

    // -----------------------------------------------------------------------
    // Page I/O
    // -----------------------------------------------------------------------

    /// Read page `page_num` from the database.
    ///
    /// The read path is:
    /// 1. Check the LRU page cache (keyed by current root + page number).
    /// 2. Resolve the page CID from the live page index.
    /// 3. Fetch page bytes from CraftOBJ.
    /// 4. Verify BLAKE3 integrity against the stored CID.
    /// 5. Populate the cache.
    ///
    /// # Errors
    /// - [`VfsError::PageNotFound`] if the page has never been written.
    /// - [`VfsError::IntegrityCheckFailed`] if the fetched data is corrupt.
    /// - [`VfsError::StoreError`] on CraftOBJ I/O failure.
    pub async fn read_page(&self, page_num: u32) -> Result<Vec<u8>> {
        let root = (*self.current_root.read()).unwrap_or_else(|| Cid::from_bytes([0u8; 32]));

        // 1. Cache lookup.
        if let Some(cached) = self.page_cache.get(&root, page_num) {
            tracing::debug!(page = page_num, cache_hit = true, "CID-VFS: read page");
            return Ok(cached);
        }
        tracing::debug!(page = page_num, cache_hit = false, "CID-VFS: read page");

        // 2. Resolve CID from page index.
        let cid = self
            .page_index
            .get(page_num)
            .ok_or(VfsError::PageNotFound(page_num))?;

        // 3. Fetch bytes from CraftOBJ.
        let bytes_opt = self
            .store
            .get(&cid)
            .await
            .map_err(|e| VfsError::StoreError(e.to_string()))?;

        let bytes: Vec<u8> = bytes_opt
            .ok_or_else(|| VfsError::StoreError(format!("CID {cid} missing from store")))?
            .to_vec();

        // 4. Verify integrity: CraftOBJ already verifies BLAKE3 on reads, but
        //    we also cross-check against the CID we looked up in the index.
        let expected_cid = Cid::from_data(&bytes);
        if expected_cid != cid {
            return Err(VfsError::IntegrityCheckFailed {
                page: page_num,
                expected: format!("{cid}"),
                actual: format!("{expected_cid}"),
            });
        }

        // 5. Cache and return.
        self.page_cache.put(&root, page_num, bytes.clone());
        Ok(bytes)
    }

    /// Buffer `data` as a dirty write for `page_num`.
    ///
    /// The page is NOT written to CraftOBJ immediately.  It is held in the
    /// dirty-page map until [`commit`](Self::commit) is called, matching
    /// SQLite's deferred write semantics.
    ///
    /// This method is synchronous — it only touches in-memory state.
    ///
    /// # Errors
    /// Returns [`VfsError::InvalidPageSize`] if `data.len()` does not match
    /// the configured page size.
    pub fn write_page(&self, page_num: u32, data: &[u8]) -> Result<()> {
        if data.len() != self.page_size {
            return Err(VfsError::InvalidPageSize(data.len()));
        }
        self.dirty_pages.lock().insert(page_num, data.to_vec());
        tracing::debug!(page = page_num, "CID-VFS: dirty page buffered");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Commit
    // -----------------------------------------------------------------------

    /// Commit all dirty pages to CraftOBJ and update the root CID.
    ///
    /// ## Steps
    /// 1. Drain `dirty_pages`.
    /// 2. For each dirty page: call `store.put(page_bytes)` which returns CID.
    /// 3. Update `page_index[page_num] = CID`.
    /// 4. Serialise the full page index.
    /// 5. Call `store.put(serialised_index)` to get the new root CID.
    /// 6. Update `page_index.root_cid` and `self.current_root`.
    /// 7. Return the new root CID.
    ///
    /// If there are no dirty pages the current root CID is returned unchanged
    /// (or [`VfsError::NoRootCid`] if the database is empty).
    ///
    /// # Errors
    /// - [`VfsError::StoreError`] on CraftOBJ failure.
    /// - [`VfsError::NoRootCid`] on an empty database with no dirty pages.
    pub async fn commit(&self) -> Result<Cid> {
        let dirty: HashMap<u32, Vec<u8>> = {
            let mut guard = self.dirty_pages.lock();
            std::mem::take(&mut *guard)
        };

        if dirty.is_empty() {
            return (*self.current_root.read()).ok_or(VfsError::NoRootCid);
        }

        let dirty_count = dirty.len();

        // Steps 2-3: store each dirty page, get back CID, update index.
        for (page_num, page_bytes) in dirty {
            let cid = self
                .store
                .put(&page_bytes)
                .await
                .map_err(|e| VfsError::StoreError(e.to_string()))?;

            self.page_index.set(page_num, cid);
        }

        // Steps 4-5: serialise index, store it, get root CID.
        let serialised = self.page_index.serialize();
        let new_root = self
            .store
            .put(&serialised)
            .await
            .map_err(|e| VfsError::StoreError(e.to_string()))?;

        // Step 6: update in-memory root.
        self.page_index.set_root(new_root);
        *self.current_root.write() = Some(new_root);

        tracing::info!(
            dirty_count = dirty_count,
            root = %new_root,
            "CID-VFS: commit",
        );

        Ok(new_root)
    }

    // -----------------------------------------------------------------------
    // Snapshot isolation
    // -----------------------------------------------------------------------

    /// Pin the current database state as an immutable [`Snapshot`].
    ///
    /// The snapshot captures the root CID and the complete page-number → CID
    /// mapping at this instant.  Subsequent commits do not affect the
    /// snapshot's view of the data.
    ///
    /// # Errors
    /// Returns [`VfsError::NoRootCid`] if the database is empty (no commits).
    pub fn snapshot(&self) -> Result<Snapshot> {
        let root = (*self.current_root.read()).ok_or(VfsError::NoRootCid)?;
        let snap = Snapshot::new(root, Arc::clone(&self.page_index));
        tracing::trace!(root = %root, "CID-VFS: snapshot pinned");
        Ok(snap)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Return the current root CID, if any.
    pub fn current_root(&self) -> Option<Cid> {
        *self.current_root.read()
    }

    /// Return the configured page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Return a reference to the live [`PageIndex`].
    pub fn page_index(&self) -> &Arc<PageIndex> {
        &self.page_index
    }

    /// Return a reference to the [`PageCache`].
    pub fn page_cache(&self) -> &Arc<PageCache> {
        &self.page_cache
    }

    /// Return the number of pages tracked in the live index.
    pub fn page_count(&self) -> usize {
        self.page_index.page_count()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_obj::ContentAddressedStore;
    use tempfile::tempdir;

    fn make_store() -> (Arc<ContentAddressedStore>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(dir.path(), 64).unwrap());
        (store, dir)
    }

    fn make_vfs() -> (CidVfs, tempfile::TempDir) {
        let (store, dir) = make_store();
        let vfs = CidVfs::new(store, DEFAULT_PAGE_SIZE).expect("VFS construction should succeed");
        (vfs, dir)
    }

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; DEFAULT_PAGE_SIZE]
    }

    #[test]
    fn invalid_page_size_rejected() {
        let dir = tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(dir.path(), 4).unwrap());
        assert!(CidVfs::new(store.clone(), 100).is_err());
        assert!(CidVfs::new(store.clone(), 16385).is_err());
        assert!(CidVfs::new(store, DEFAULT_PAGE_SIZE).is_ok());
    }

    #[tokio::test]
    async fn write_and_read_page_round_trip() {
        let (vfs, _dir) = make_vfs();
        let data = page(0xAB);
        vfs.write_page(0, &data).unwrap();
        vfs.commit().await.unwrap();

        let read_back = vfs.read_page(0).await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn read_missing_page_returns_error() {
        let (vfs, _dir) = make_vfs();
        vfs.write_page(0, &page(0x01)).unwrap();
        vfs.commit().await.unwrap();
        assert!(matches!(vfs.read_page(99).await, Err(VfsError::PageNotFound(99))));
    }

    #[tokio::test]
    async fn commit_returns_stable_cid_for_identical_content() {
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        let store1 = Arc::new(ContentAddressedStore::new(dir1.path(), 64).unwrap());
        let store2 = Arc::new(ContentAddressedStore::new(dir2.path(), 64).unwrap());

        let vfs1 = CidVfs::new(Arc::clone(&store1), DEFAULT_PAGE_SIZE).unwrap();
        let vfs2 = CidVfs::new(Arc::clone(&store2), DEFAULT_PAGE_SIZE).unwrap();

        let data = page(0x55);
        vfs1.write_page(0, &data).unwrap();
        vfs2.write_page(0, &data).unwrap();

        let root1 = vfs1.commit().await.unwrap();
        let root2 = vfs2.commit().await.unwrap();
        assert_eq!(root1, root2, "identical content must produce identical root CID");
    }

    #[tokio::test]
    async fn snapshot_isolation() {
        let (vfs, _dir) = make_vfs();
        vfs.write_page(0, &page(0x01)).unwrap();
        vfs.commit().await.unwrap();

        let snap = vfs.snapshot().unwrap();
        let snap_page0_cid = snap.resolve_page(0).unwrap();

        vfs.write_page(0, &page(0x02)).unwrap();
        vfs.commit().await.unwrap();

        // Snapshot must still see old CID.
        assert_eq!(snap.resolve_page(0), Some(snap_page0_cid));
    }

    #[test]
    fn snapshot_on_empty_vfs_errors() {
        let (vfs, _dir) = make_vfs();
        assert!(matches!(vfs.snapshot(), Err(VfsError::NoRootCid)));
    }

    #[tokio::test]
    async fn page_cache_hit_on_second_read() {
        let (vfs, _dir) = make_vfs();
        vfs.write_page(1, &page(0xCC)).unwrap();
        vfs.commit().await.unwrap();

        vfs.read_page(1).await.unwrap(); // miss
        vfs.read_page(1).await.unwrap(); // hit

        let cache = vfs.page_cache();
        assert!(cache.total_hits() >= 1);
    }
}
