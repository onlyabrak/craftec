//! [`CodedPieceIndex`] — maps content CIDs to their RLNC coded-piece CIDs.
//!
//! When raw data is stored via CraftOBJ, the RLNC pipeline encodes it into
//! coded pieces and stores each piece back in CraftOBJ.  This module tracks
//! the mapping so [`PieceRequest`](craftec_types::WireMessage::PieceRequest)
//! can serve real coded pieces instead of identity-coded fallbacks.
//!
//! ## Recursive encoding prevention
//!
//! Storing coded pieces triggers `CidWritten` events.  The `piece_cids` set
//! tracks which CIDs are piece artifacts so the event handler can skip
//! re-encoding them.

use craftec_health::PieceCidLookup;
use craftec_types::Cid;
use dashmap::{DashMap, DashSet};

/// Index mapping content CIDs to their RLNC coded-piece CIDs.
pub struct CodedPieceIndex {
    /// content CID → Vec of piece CIDs stored in CraftOBJ.
    index: DashMap<Cid, Vec<Cid>>,
    /// Set of all piece CIDs (for recursive-encoding prevention).
    piece_cids: DashSet<Cid>,
}

impl CodedPieceIndex {
    /// Create a new, empty index.
    pub fn new() -> Self {
        Self {
            index: DashMap::new(),
            piece_cids: DashSet::new(),
        }
    }

    /// Record that `content_cid` has been encoded into `piece_cids`.
    pub fn insert(&self, content_cid: Cid, piece_cids: Vec<Cid>) {
        self.index.insert(content_cid, piece_cids);
    }

    /// Mark a CID as a coded-piece artifact (prevents recursive encoding).
    pub fn mark_piece_cid(&self, cid: Cid) {
        self.piece_cids.insert(cid);
    }

    /// Check if a CID is a coded-piece artifact.
    pub fn is_piece_cid(&self, cid: &Cid) -> bool {
        self.piece_cids.contains(cid)
    }

    /// Get the coded-piece CIDs for a content CID.
    pub fn get(&self, content_cid: &Cid) -> Option<Vec<Cid>> {
        self.index.get(content_cid).map(|v| v.clone())
    }
}

impl PieceCidLookup for CodedPieceIndex {
    fn piece_cids(&self, content_cid: &Cid) -> Option<Vec<Cid>> {
        self.get(content_cid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_piece_index_insert_and_get() {
        let idx = CodedPieceIndex::new();
        let content = Cid::from_data(b"content");
        let p1 = Cid::from_data(b"piece1");
        let p2 = Cid::from_data(b"piece2");

        idx.insert(content, vec![p1, p2]);

        let pieces = idx.get(&content).unwrap();
        assert_eq!(pieces.len(), 2);
        assert!(pieces.contains(&p1));
        assert!(pieces.contains(&p2));
    }

    #[test]
    fn piece_cid_tracking() {
        let idx = CodedPieceIndex::new();
        let pcid = Cid::from_data(b"coded-piece");

        assert!(!idx.is_piece_cid(&pcid));
        idx.mark_piece_cid(pcid);
        assert!(idx.is_piece_cid(&pcid));
    }

    #[test]
    fn get_missing_returns_none() {
        let idx = CodedPieceIndex::new();
        let cid = Cid::from_data(b"nonexistent");
        assert!(idx.get(&cid).is_none());
    }
}
