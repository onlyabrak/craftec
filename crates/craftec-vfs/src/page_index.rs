//! Page index — maps SQLite page numbers to content-addressed CIDs.
//!
//! The [`PageIndex`] is the authoritative mapping of `page_number → CID`
//! for a single CID-VFS database instance.  It is fully in-memory and is
//! serialised to CraftOBJ on every commit to produce a new *root CID*
//! that represents the complete database state.
//!
//! ## Snapshot semantics
//! A snapshot is simply a cloned, immutable view of the `entries` map at a
//! particular point in time.  Because CraftOBJ is append-only, the underlying
//! page objects never mutate; only the index evolves.

use std::collections::HashMap;

use craftec_types::Cid;
use parking_lot::RwLock;

use crate::error::{Result, VfsError};

/// Maps SQLite page numbers to CIDs in content-addressed storage.
///
/// Thread-safe via interior `RwLock`s; multiple readers may hold the read
/// lock concurrently while a commit holds the write lock briefly.
pub struct PageIndex {
    /// Core mapping: page number → CID.
    entries: RwLock<HashMap<u32, Cid>>,
    /// CID of the most recently committed serialised page index.
    root_cid: RwLock<Option<Cid>>,
}

impl PageIndex {
    /// Create an empty [`PageIndex`].
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            root_cid: RwLock::new(None),
        }
    }

    /// Look up the CID for `page_num`.
    ///
    /// Returns `None` if the page has never been written.
    pub fn get(&self, page_num: u32) -> Option<Cid> {
        self.entries.read().get(&page_num).copied()
    }

    /// Associate `page_num` with `cid` in the index.
    ///
    /// Called during commit after a dirty page is stored in CraftOBJ.
    pub fn set(&self, page_num: u32, cid: Cid) {
        self.entries.write().insert(page_num, cid);
    }

    /// Remove a page from the index.
    ///
    /// Used when pages are freed (e.g., after a `DROP TABLE` or `VACUUM`).
    pub fn remove(&self, page_num: u32) {
        self.entries.write().remove(&page_num);
    }

    /// Return the root CID of the last committed page index.
    ///
    /// `None` until the first [`commit`](crate::vfs::CidVfs::commit) completes.
    pub fn root(&self) -> Option<Cid> {
        *self.root_cid.read()
    }

    /// Update the stored root CID after a successful commit.
    pub(crate) fn set_root(&self, cid: Cid) {
        *self.root_cid.write() = Some(cid);
    }

    /// Serialise the full page-number → CID map into a compact byte vector.
    ///
    /// The format is a length-prefixed sequence of `(u32, [u8; 32])` pairs
    /// encoded as little-endian bytes.  This is intentionally simple so that
    /// the binary layout is stable across compilations.
    ///
    /// The resulting bytes are BLAKE3-hashed to form the root CID on commit.
    pub fn serialize(&self) -> Vec<u8> {
        let entries = self.entries.read();
        // Format: [entry_count: u32 LE] followed by [page_num: u32 LE][cid: 32 bytes]...
        let mut buf = Vec::with_capacity(4 + entries.len() * 36);
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());

        // Sort by page number for deterministic output.
        let mut pairs: Vec<(u32, Cid)> = entries.iter().map(|(&k, &v)| (k, v)).collect();
        pairs.sort_by_key(|(k, _)| *k);

        for (page_num, cid) in pairs {
            buf.extend_from_slice(&page_num.to_le_bytes());
            buf.extend_from_slice(cid.as_bytes());
        }
        buf
    }

    /// Deserialise a [`PageIndex`] from bytes previously produced by
    /// [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`VfsError::SerializationError`] if the byte slice is truncated
    /// or otherwise malformed.
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if data.len() < 4 {
            return Err(VfsError::SerializationError(
                "page index too short to read entry count".into(),
            ));
        }
        let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let expected_len = 4 + count * 36;
        if data.len() < expected_len {
            return Err(VfsError::SerializationError(format!(
                "page index truncated: expected {expected_len} bytes, got {}",
                data.len()
            )));
        }

        let mut entries = HashMap::with_capacity(count);
        for i in 0..count {
            let offset = 4 + i * 36;
            let page_num = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            let cid_bytes: [u8; 32] = data[offset + 4..offset + 36]
                .try_into()
                .map_err(|_| VfsError::SerializationError("CID slice has wrong length".into()))?;
            let cid = Cid::from_bytes(cid_bytes);
            entries.insert(page_num, cid);
        }

        Ok(Self {
            entries: RwLock::new(entries),
            root_cid: RwLock::new(None),
        })
    }

    /// Return the number of pages currently tracked by this index.
    pub fn page_count(&self) -> usize {
        self.entries.read().len()
    }

    /// Return a snapshot copy of all current entries.
    ///
    /// Used when producing a [`Snapshot`](crate::snapshot::Snapshot).
    pub fn snapshot_entries(&self) -> HashMap<u32, Cid> {
        self.entries.read().clone()
    }
}

impl Default for PageIndex {
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

    fn fake_cid(seed: u8) -> Cid {
        Cid::from_bytes([seed; 32])
    }

    #[test]
    fn get_set_round_trip() {
        let idx = PageIndex::new();
        assert_eq!(idx.get(1), None);
        idx.set(1, fake_cid(0xAA));
        assert_eq!(idx.get(1), Some(fake_cid(0xAA)));
    }

    #[test]
    fn page_count_reflects_inserts() {
        let idx = PageIndex::new();
        assert_eq!(idx.page_count(), 0);
        idx.set(0, fake_cid(0x01));
        idx.set(1, fake_cid(0x02));
        assert_eq!(idx.page_count(), 2);
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let idx = PageIndex::new();
        idx.set(0, fake_cid(0x00));
        idx.set(5, fake_cid(0x05));
        idx.set(10, fake_cid(0x0A));

        let bytes = idx.serialize();
        let idx2 = PageIndex::deserialize(&bytes).expect("deserialize should succeed");

        assert_eq!(idx2.page_count(), 3);
        assert_eq!(idx2.get(0), Some(fake_cid(0x00)));
        assert_eq!(idx2.get(5), Some(fake_cid(0x05)));
        assert_eq!(idx2.get(10), Some(fake_cid(0x0A)));
    }

    #[test]
    fn deserialize_rejects_truncated_data() {
        let result = PageIndex::deserialize(&[0u8; 2]);
        assert!(result.is_err());
    }

    #[test]
    fn remove_drops_entry() {
        let idx = PageIndex::new();
        idx.set(3, fake_cid(0x03));
        assert_eq!(idx.page_count(), 1);
        idx.remove(3);
        assert_eq!(idx.page_count(), 0);
        assert_eq!(idx.get(3), None);
    }

    #[test]
    fn root_cid_starts_none() {
        let idx = PageIndex::new();
        assert_eq!(idx.root(), None);
        let cid = fake_cid(0xFF);
        idx.set_root(cid);
        assert_eq!(idx.root(), Some(cid));
    }
}
