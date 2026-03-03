//! Wire protocol message types, serialized with [postcard].
//!
//! All messages exchanged over the Craftec QUIC transport are
//! [`WireMessage`] values encoded with `postcard` (compact, no-alloc
//! compatible binary format built on top of serde).
//!
//! Use [`encode`] and [`decode`] for framing.

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::cid::Cid;
use crate::error::{CraftecError, Result};
use crate::identity::{NodeId, Signature};
use crate::piece::CodedPiece;

/// Every message sent or received over the Craftec wire protocol.
///
/// Variants cover liveness checks, piece exchange, SWIM gossip, mutable-data
/// writes, and health reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireMessage {
    // ── Liveness ──────────────────────────────────────────────────────

    /// Echo request.
    Ping {
        /// Arbitrary caller-chosen nonce echoed back in the `Pong`.
        nonce: u64,
    },

    /// Echo response.
    Pong {
        /// The nonce from the corresponding `Ping`.
        nonce: u64,
    },

    // ── Piece exchange ─────────────────────────────────────────────────

    /// Request specific coded pieces for a content identifier.
    PieceRequest {
        /// The CID whose pieces are being requested.
        cid: Cid,
        /// Indices of the desired pieces (within the generation).
        piece_indices: Vec<u32>,
    },

    /// Deliver coded pieces in response to a [`WireMessage::PieceRequest`].
    PieceResponse {
        /// The actual coded pieces.
        pieces: Vec<CodedPiece>,
    },

    // ── Discovery ─────────────────────────────────────────────────────

    /// Announce that this node stores pieces for `cid`.
    ProviderAnnounce {
        /// The announced CID.
        cid: Cid,
        /// The announcing node's ID.
        node_id: NodeId,
    },

    // ── Mutable data ──────────────────────────────────────────────────

    /// A signed write to mutable content-addressed storage.
    SignedWrite {
        /// Serialized payload (opaque bytes).
        payload: Vec<u8>,
        /// Ed25519 signature over `payload`.
        signature: Signature,
        /// The node that produced this write.
        writer: NodeId,
        /// Compare-and-swap version (monotone counter).
        cas_version: u64,
    },

    // ── SWIM gossip ───────────────────────────────────────────────────

    /// A node joining the SWIM membership ring.
    SwimJoin {
        /// Node that is joining.
        node_id: NodeId,
        /// UDP/QUIC port the joining node listens on.
        listen_port: u16,
    },

    /// Confirm a node is alive, with its current incarnation number.
    SwimAlive {
        /// The node confirmed alive.
        node_id: NodeId,
        /// Incarnation counter (bumped on refutation).
        incarnation: u64,
    },

    /// Suspect a node of failure.
    SwimSuspect {
        /// The suspected node.
        node_id: NodeId,
        /// Last known incarnation of the suspected node.
        incarnation: u64,
        /// Node reporting the suspicion.
        from: NodeId,
    },

    /// Declare a node dead.
    SwimDead {
        /// The node declared dead.
        node_id: NodeId,
        /// Incarnation at time of declaration.
        incarnation: u64,
        /// Node reporting the death.
        from: NodeId,
    },

    // ── Health ────────────────────────────────────────────────────────

    /// Report the replication health of a stored CID.
    HealthReport {
        /// The CID being reported on.
        cid: Cid,
        /// How many distinct coded pieces this node currently stores.
        available_pieces: u32,
        /// Target number of pieces for this CID.
        target_pieces: u32,
    },

    // ── SWIM probe ────────────────────────────────────────────────────

    /// SWIM probe ping with piggybacked membership gossip.
    SwimPing {
        /// The node sending the ping.
        from: NodeId,
        /// Piggybacked membership updates.
        piggyback: Vec<WireMessage>,
    },
}

impl WireMessage {
    /// Return a human-readable name for the message variant (for logging).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Ping { .. } => "Ping",
            Self::Pong { .. } => "Pong",
            Self::PieceRequest { .. } => "PieceRequest",
            Self::PieceResponse { .. } => "PieceResponse",
            Self::ProviderAnnounce { .. } => "ProviderAnnounce",
            Self::SignedWrite { .. } => "SignedWrite",
            Self::SwimJoin { .. } => "SwimJoin",
            Self::SwimAlive { .. } => "SwimAlive",
            Self::SwimSuspect { .. } => "SwimSuspect",
            Self::SwimDead { .. } => "SwimDead",
            Self::HealthReport { .. } => "HealthReport",
            Self::SwimPing { .. } => "SwimPing",
        }
    }
}

/// Encode a [`WireMessage`] to bytes using [postcard].
///
/// The result is a compact variable-length byte string suitable for framing
/// over a length-prefixed QUIC stream.
pub fn encode(msg: &WireMessage) -> Result<Vec<u8>> {
    trace!(msg_variant = ?std::mem::discriminant(msg), "encoding WireMessage");
    let bytes = postcard::to_allocvec(msg).map_err(|e| {
        CraftecError::SerializationError(format!("postcard encode error: {e}"))
    })?;
    debug!(
        msg_variant = ?std::mem::discriminant(msg),
        encoded_len = bytes.len(),
        "encoded WireMessage"
    );
    Ok(bytes)
}

/// Decode a [`WireMessage`] from bytes using [postcard].
pub fn decode(data: &[u8]) -> Result<WireMessage> {
    trace!(data_len = data.len(), "decoding WireMessage");
    let msg: WireMessage = postcard::from_bytes(data).map_err(|e| {
        CraftecError::SerializationError(format!("postcard decode error: {e}"))
    })?;
    debug!(
        msg_variant = ?std::mem::discriminant(&msg),
        "decoded WireMessage"
    );
    Ok(msg)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_pong_round_trip() {
        let msg = WireMessage::Ping { nonce: 42 };
        let bytes = encode(&msg).unwrap();
        let decoded = decode(&bytes).unwrap();
        match decoded {
            WireMessage::Ping { nonce } => assert_eq!(nonce, 42),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn piece_request_round_trip() {
        let cid = Cid::from_data(b"test");
        let msg = WireMessage::PieceRequest {
            cid,
            piece_indices: vec![0, 1, 2],
        };
        let bytes = encode(&msg).unwrap();
        let decoded = decode(&bytes).unwrap();
        match decoded {
            WireMessage::PieceRequest { cid: c, piece_indices } => {
                assert_eq!(c, cid);
                assert_eq!(piece_indices, vec![0, 1, 2]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn swim_dead_round_trip() {
        use crate::identity::NodeKeypair;
        let node_id = NodeKeypair::generate().node_id();
        let from = NodeKeypair::generate().node_id();
        let msg = WireMessage::SwimDead {
            node_id,
            incarnation: 7,
            from,
        };
        let bytes = encode(&msg).unwrap();
        let decoded = decode(&bytes).unwrap();
        match decoded {
            WireMessage::SwimDead { incarnation, .. } => assert_eq!(incarnation, 7),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn decode_garbage_returns_error() {
        let result = decode(&[0xFF, 0xFE, 0x00]);
        assert!(result.is_err());
    }
}
