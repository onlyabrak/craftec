//! Snapshot isolation for CID-VFS.
//!
//! A [`Snapshot`] pins a particular root CID at the moment a query begins,
//! guaranteeing that subsequent page reads see a consistent database state
//! regardless of concurrent commits.
//!
//! ## How it works
//! 1. The caller obtains a snapshot via [`CidVfs::snapshot`].
//! 2. The snapshot stores the current root CID and a reference to the page
//!    index at that point in time.
//! 3. All page reads within the snapshot resolve CIDs against the *pinned*
//!    page-index entries rather than the live index.
//! 4. When the snapshot is dropped, the pinned root becomes eligible for
//!    garbage collection (future work).
//!
//! Because CraftOBJ is append-only, the underlying page bytes for the pinned
//! root remain available indefinitely — old versions are never overwritten.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use craftec_types::Cid;

use crate::page_index::PageIndex;

/// An immutable, point-in-time view of a CID-VFS database.
///
/// Obtain snapshots via [`CidVfs::snapshot`](crate::vfs::CidVfs::snapshot).
/// Snapshots are cheaply cloneable and `Send + Sync`.
#[derive(Clone)]
pub struct Snapshot {
    /// The root CID that identifies this exact database state.
    pub root_cid: Cid,
    /// Frozen copy of page-number → CID entries at snapshot time.
    entries: Arc<HashMap<u32, Cid>>,
    /// Wall-clock time at which the snapshot was created.
    pub created_at: Instant,
    /// Reference to the live page index (used for metrics / logging only).
    page_index: Arc<PageIndex>,
}

impl Snapshot {
    /// Create a new snapshot from the current state of `page_index`.
    ///
    /// `root_cid` must be the root CID returned by the most recent commit.
    pub fn new(root_cid: Cid, page_index: Arc<PageIndex>) -> Self {
        let entries = Arc::new(page_index.snapshot_entries());
        let created_at = Instant::now();
        tracing::trace!(
            root = %root_cid,
            page_count = entries.len(),
            "CID-VFS: snapshot pinned",
        );
        Self {
            root_cid,
            entries,
            created_at,
            page_index,
        }
    }

    /// Resolve `page_num` to its CID within this snapshot.
    ///
    /// Returns `None` if the page did not exist at snapshot time.
    pub fn resolve_page(&self, page_num: u32) -> Option<Cid> {
        self.entries.get(&page_num).copied()
    }

    /// Number of pages visible in this snapshot.
    pub fn page_count(&self) -> usize {
        self.entries.len()
    }

    /// Age of this snapshot since it was pinned.
    pub fn age(&self) -> std::time::Duration {
        self.created_at.elapsed()
    }

    /// Return the root CID identifying this snapshot.
    pub fn root(&self) -> Cid {
        self.root_cid
    }

    /// Return a reference to the underlying live [`PageIndex`].
    ///
    /// Useful for diagnostic or metrics purposes only — do **not** use for
    /// reads within this snapshot (use [`resolve_page`](Self::resolve_page)
    /// instead to maintain isolation).
    pub fn live_index(&self) -> &Arc<PageIndex> {
        &self.page_index
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("root_cid", &self.root_cid)
            .field("page_count", &self.entries.len())
            .field("age_ms", &self.created_at.elapsed().as_millis())
            .finish()
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

    #[test]
    fn snapshot_captures_entries_at_pin_time() {
        let idx = Arc::new(PageIndex::new());
        idx.set(0, cid(0x01));
        idx.set(1, cid(0x02));

        let snap = Snapshot::new(cid(0xFF), Arc::clone(&idx));
        assert_eq!(snap.page_count(), 2);
        assert_eq!(snap.resolve_page(0), Some(cid(0x01)));
        assert_eq!(snap.resolve_page(1), Some(cid(0x02)));

        // Mutations after snapshot creation must NOT be visible.
        idx.set(2, cid(0x03));
        assert_eq!(snap.resolve_page(2), None, "snapshot must be isolated");
        assert_eq!(snap.page_count(), 2);
    }

    #[test]
    fn root_cid_matches() {
        let idx = Arc::new(PageIndex::new());
        let snap = Snapshot::new(cid(0xAB), Arc::clone(&idx));
        assert_eq!(snap.root(), cid(0xAB));
    }

    #[test]
    fn age_increases_over_time() {
        let idx = Arc::new(PageIndex::new());
        let snap = Snapshot::new(cid(0x01), Arc::clone(&idx));
        let d1 = snap.age();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let d2 = snap.age();
        assert!(d2 > d1);
    }

    #[test]
    fn debug_format_does_not_panic() {
        let idx = Arc::new(PageIndex::new());
        let snap = Snapshot::new(cid(0x01), Arc::clone(&idx));
        let _ = format!("{snap:?}");
    }
}
