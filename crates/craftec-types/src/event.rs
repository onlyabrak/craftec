//! Event bus types for internal Craftec component communication.
//!
//! Components communicate asynchronously via bounded [`tokio::sync::broadcast`]
//! channels carrying [`Event`] values.  Channel capacity constants are defined
//! here so that all subscribers use consistent buffer sizes.

use crate::cid::Cid;
use crate::identity::NodeId;

// ── Channel capacity constants ─────────────────────────────────────────────

/// Broadcast channel capacity for [`Event::CidWritten`].
pub const CID_WRITTEN_CAP: usize = 256;

/// Broadcast channel capacity for [`Event::PageCommitted`].
pub const PAGE_COMMITTED_CAP: usize = 256;

/// Broadcast channel capacity for [`Event::PeerConnected`] and
/// [`Event::PeerDisconnected`].
pub const PEER_EVENT_CAP: usize = 256;

/// Broadcast channel capacity for [`Event::RepairNeeded`].
pub const REPAIR_NEEDED_CAP: usize = 256;

/// Broadcast channel capacity for [`Event::DiskWatermarkHit`].
pub const DISK_WATERMARK_CAP: usize = 64;

/// Broadcast channel capacity for [`Event::ShutdownSignal`].
pub const SHUTDOWN_CAP: usize = 8;

// ── Event enum ─────────────────────────────────────────────────────────────

/// An internal event broadcast to subsystem listeners.
///
/// Use a `tokio::sync::broadcast::Sender<Event>` to publish and
/// `broadcast::Receiver<Event>` to subscribe.  Slow receivers will see
/// [`tokio::sync::broadcast::error::RecvError::Lagged`] if they fall behind
/// the channel capacity.
#[derive(Debug, Clone)]
pub enum Event {
    /// A new CID has been fully written and is locally available.
    CidWritten {
        /// The newly written content identifier.
        cid: Cid,
    },

    /// A database page has been committed to storage.
    PageCommitted {
        /// Identifier of the database (top-level CID).
        db_id: Cid,
        /// Sequential page number within the database.
        page_num: u32,
        /// Merkle root CID covering all committed pages so far.
        root_cid: Cid,
    },

    /// A new peer connection has been established.
    PeerConnected {
        /// The connected peer's node identifier.
        node_id: NodeId,
    },

    /// An existing peer connection has been closed or lost.
    PeerDisconnected {
        /// The disconnected peer's node identifier.
        node_id: NodeId,
    },

    /// A CID has fallen below its replication target and needs repair.
    RepairNeeded {
        /// The under-replicated CID.
        cid: Cid,
        /// Current number of available coded pieces.
        available: u32,
        /// Target number of coded pieces.
        target: u32,
    },

    /// Disk usage has crossed a configured watermark.
    DiskWatermarkHit {
        /// Current disk usage as a fraction in `[0.0, 1.0]`.
        usage_percent: f64,
    },

    /// Graceful shutdown has been requested.
    ShutdownSignal,
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_capacities_are_positive() {
        assert!(CID_WRITTEN_CAP > 0);
        assert!(PAGE_COMMITTED_CAP > 0);
        assert!(PEER_EVENT_CAP > 0);
        assert!(REPAIR_NEEDED_CAP > 0);
        assert!(DISK_WATERMARK_CAP > 0);
        assert!(SHUTDOWN_CAP > 0);
    }

    #[test]
    fn event_clone() {
        let cid = Cid::from_data(b"test");
        let ev = Event::CidWritten { cid };
        let cloned = ev.clone();
        match cloned {
            Event::CidWritten { cid: c } => assert_eq!(c, cid),
            _ => panic!("wrong variant after clone"),
        }
    }

    #[test]
    fn repair_needed_fields() {
        let cid = Cid::from_data(b"under-replicated");
        let ev = Event::RepairNeeded {
            cid,
            available: 5,
            target: 10,
        };
        match ev {
            Event::RepairNeeded {
                available, target, ..
            } => {
                assert_eq!(available, 5);
                assert_eq!(target, 10);
            }
            _ => panic!("wrong variant"),
        }
    }
}
