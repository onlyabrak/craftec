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
        /// Correlation ID for matching request to response (T10).
        request_id: u64,
    },

    /// Deliver coded pieces in response to a [`WireMessage::PieceRequest`].
    PieceResponse {
        /// The actual coded pieces.
        pieces: Vec<CodedPiece>,
        /// Echoed correlation ID from the corresponding [`PieceRequest`] (T10).
        request_id: u64,
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
        /// Probe nonce for ack correlation.
        nonce: u64,
        /// Piggybacked membership updates.
        piggyback: Vec<WireMessage>,
    },

    /// SWIM probe ack — confirms liveness in response to a `SwimPing`.
    SwimPingAck {
        /// The node sending the ack.
        from: NodeId,
        /// Echoed probe nonce from the corresponding `SwimPing`.
        nonce: u64,
        /// Current incarnation of the ack sender.
        incarnation: u64,
    },
}

/// Wire protocol version byte (v1 includes HLC timestamp).
pub const WIRE_VERSION: u8 = 1;

/// Size of the v1 frame header: `[type_tag:4 | version:1 | hlc_ts:8 | payload_len:4]`.
pub const FRAME_HEADER_SIZE: usize = 17;

/// Size of the v0 frame header (for backward compat): `[type_tag:4 | version:1 | payload_len:4]`.
pub const FRAME_HEADER_V0_SIZE: usize = 9;

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
            Self::SwimPingAck { .. } => "SwimPingAck",
        }
    }

    /// Return the numeric type tag for this message variant.
    pub fn type_tag(&self) -> u32 {
        match self {
            Self::Ping { .. } => 0x0001,
            Self::Pong { .. } => 0x0002,
            Self::PieceRequest { .. } => 0x0010,
            Self::PieceResponse { .. } => 0x0011,
            Self::ProviderAnnounce { .. } => 0x0020,
            Self::SignedWrite { .. } => 0x0030,
            Self::SwimJoin { .. } => 0x0100,
            Self::SwimAlive { .. } => 0x0101,
            Self::SwimSuspect { .. } => 0x0102,
            Self::SwimDead { .. } => 0x0103,
            Self::SwimPing { .. } => 0x0104,
            Self::SwimPingAck { .. } => 0x0105,
            Self::HealthReport { .. } => 0x0040,
        }
    }
}

/// Encode a [`WireMessage`] to bytes using [postcard].
///
/// The result is a compact variable-length byte string suitable for framing
/// over a length-prefixed QUIC stream.
pub fn encode(msg: &WireMessage) -> Result<Vec<u8>> {
    trace!(msg_variant = ?std::mem::discriminant(msg), "encoding WireMessage");
    let bytes = postcard::to_allocvec(msg)
        .map_err(|e| CraftecError::SerializationError(format!("postcard encode error: {e}")))?;
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
    let msg: WireMessage = postcard::from_bytes(data)
        .map_err(|e| CraftecError::SerializationError(format!("postcard decode error: {e}")))?;
    debug!(
        msg_variant = ?std::mem::discriminant(&msg),
        "decoded WireMessage"
    );
    Ok(msg)
}

/// Encode a [`WireMessage`] with a 17-byte v1 frame header including HLC timestamp.
///
/// Frame layout v1: `[type_tag:u32 BE | version:u8(=1) | hlc_ts:u64 BE | payload_len:u32 BE | postcard payload]`
///
/// Use `hlc_ts = 0` when no HLC is available (e.g., during tests).
pub fn encode_framed(msg: &WireMessage) -> Result<Vec<u8>> {
    encode_framed_with_hlc(msg, 0)
}

/// Encode a [`WireMessage`] with an explicit HLC timestamp.
pub fn encode_framed_with_hlc(msg: &WireMessage, hlc_ts: u64) -> Result<Vec<u8>> {
    let payload = postcard::to_allocvec(msg)
        .map_err(|e| CraftecError::SerializationError(format!("postcard encode error: {e}")))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
    frame.extend_from_slice(&msg.type_tag().to_be_bytes()); // 4 bytes
    frame.push(WIRE_VERSION); // 1 byte
    frame.extend_from_slice(&hlc_ts.to_be_bytes()); // 8 bytes
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes()); // 4 bytes
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode a framed [`WireMessage`], returning (message, hlc_timestamp).
///
/// Supports both v0 (9-byte header, no HLC) and v1 (17-byte header with HLC).
/// V0 frames return `hlc_timestamp = 0`.
pub fn decode_framed(data: &[u8]) -> Result<WireMessage> {
    let (msg, _ts) = decode_framed_with_hlc(data)?;
    Ok(msg)
}

/// Decode a framed [`WireMessage`] and return the HLC timestamp.
pub fn decode_framed_with_hlc(data: &[u8]) -> Result<(WireMessage, u64)> {
    if data.len() < FRAME_HEADER_V0_SIZE {
        return Err(CraftecError::SerializationError(format!(
            "frame too short: {} bytes (need at least {})",
            data.len(),
            FRAME_HEADER_V0_SIZE
        )));
    }

    let version = data[4];

    match version {
        0 => {
            // V0: [type_tag:4 | version:1 | payload_len:4 | payload]
            let payload_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]) as usize;
            if data.len() < FRAME_HEADER_V0_SIZE + payload_len {
                return Err(CraftecError::SerializationError(format!(
                    "v0 frame truncated: have {} bytes, need {}",
                    data.len(),
                    FRAME_HEADER_V0_SIZE + payload_len
                )));
            }
            let msg: WireMessage = postcard::from_bytes(
                &data[FRAME_HEADER_V0_SIZE..FRAME_HEADER_V0_SIZE + payload_len],
            )
            .map_err(|e| CraftecError::SerializationError(format!("postcard decode error: {e}")))?;
            Ok((msg, 0))
        }
        1 => {
            // V1: [type_tag:4 | version:1 | hlc_ts:8 | payload_len:4 | payload]
            if data.len() < FRAME_HEADER_SIZE {
                return Err(CraftecError::SerializationError(format!(
                    "v1 frame too short: {} bytes (need at least {})",
                    data.len(),
                    FRAME_HEADER_SIZE
                )));
            }
            let hlc_ts = u64::from_be_bytes([
                data[5], data[6], data[7], data[8], data[9], data[10], data[11], data[12],
            ]);
            let payload_len = u32::from_be_bytes([data[13], data[14], data[15], data[16]]) as usize;
            if data.len() < FRAME_HEADER_SIZE + payload_len {
                return Err(CraftecError::SerializationError(format!(
                    "v1 frame truncated: have {} bytes, need {}",
                    data.len(),
                    FRAME_HEADER_SIZE + payload_len
                )));
            }
            let msg: WireMessage =
                postcard::from_bytes(&data[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + payload_len])
                    .map_err(|e| {
                        CraftecError::SerializationError(format!("postcard decode error: {e}"))
                    })?;
            Ok((msg, hlc_ts))
        }
        _ => Err(CraftecError::SerializationError(format!(
            "unsupported wire version: {} (supported: 0, 1)",
            version
        ))),
    }
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
            request_id: 42,
        };
        let bytes = encode(&msg).unwrap();
        let decoded = decode(&bytes).unwrap();
        match decoded {
            WireMessage::PieceRequest {
                cid: c,
                piece_indices,
                request_id,
            } => {
                assert_eq!(c, cid);
                assert_eq!(piece_indices, vec![0, 1, 2]);
                assert_eq!(request_id, 42);
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

    #[test]
    fn framed_round_trip_all_variants() {
        use crate::identity::NodeKeypair;
        let nid = NodeKeypair::generate().node_id();
        let cid = Cid::from_data(b"test");

        let variants: Vec<WireMessage> = vec![
            WireMessage::Ping { nonce: 1 },
            WireMessage::Pong { nonce: 2 },
            WireMessage::PieceRequest {
                cid,
                piece_indices: vec![0],
                request_id: 1,
            },
            WireMessage::PieceResponse {
                pieces: vec![],
                request_id: 1,
            },
            WireMessage::ProviderAnnounce { cid, node_id: nid },
            WireMessage::SignedWrite {
                payload: vec![0],
                signature: NodeKeypair::generate().sign(b"test"),
                writer: nid,
                cas_version: 0,
            },
            WireMessage::SwimJoin {
                node_id: nid,
                listen_port: 9000,
            },
            WireMessage::SwimAlive {
                node_id: nid,
                incarnation: 0,
            },
            WireMessage::SwimSuspect {
                node_id: nid,
                incarnation: 0,
                from: nid,
            },
            WireMessage::SwimDead {
                node_id: nid,
                incarnation: 0,
                from: nid,
            },
            WireMessage::SwimPing {
                from: nid,
                nonce: 99,
                piggyback: vec![],
            },
            WireMessage::SwimPingAck {
                from: nid,
                nonce: 99,
                incarnation: 0,
            },
            WireMessage::HealthReport {
                cid,
                available_pieces: 5,
                target_pieces: 10,
            },
        ];

        for msg in &variants {
            let framed = encode_framed(msg).unwrap();
            assert!(framed.len() >= FRAME_HEADER_SIZE);
            let decoded = decode_framed(&framed).unwrap();
            assert_eq!(decoded.type_name(), msg.type_name());
        }
    }

    #[test]
    fn decode_framed_wrong_version_fails() {
        let msg = WireMessage::Ping { nonce: 1 };
        let mut framed = encode_framed(&msg).unwrap();
        framed[4] = 0xFF; // corrupt version byte
        assert!(decode_framed(&framed).is_err());
    }

    #[test]
    fn decode_framed_truncated_fails() {
        // Less than minimum header size
        assert!(decode_framed(&[0; 5]).is_err());

        // V1 header says 100 bytes payload, but we only have 17
        let mut data = vec![0u8; FRAME_HEADER_SIZE];
        data[4] = 1; // version 1
        data[13..17].copy_from_slice(&100u32.to_be_bytes());
        assert!(decode_framed(&data).is_err());
    }

    #[test]
    fn framed_v1_with_hlc() {
        let msg = WireMessage::Ping { nonce: 42 };
        let hlc_ts = 0x0001_0203_0405_0607u64;
        let framed = encode_framed_with_hlc(&msg, hlc_ts).unwrap();
        assert!(framed.len() >= FRAME_HEADER_SIZE);
        assert_eq!(framed[4], 1, "version should be 1");
        let (decoded, decoded_ts) = decode_framed_with_hlc(&framed).unwrap();
        assert_eq!(decoded_ts, hlc_ts);
        assert_eq!(decoded.type_name(), "Ping");
    }

    #[test]
    fn framed_v0_backward_compat() {
        // Manually construct a v0 frame.
        let msg = WireMessage::Pong { nonce: 99 };
        let payload = postcard::to_allocvec(&msg).unwrap();
        let mut frame = Vec::with_capacity(FRAME_HEADER_V0_SIZE + payload.len());
        frame.extend_from_slice(&msg.type_tag().to_be_bytes());
        frame.push(0u8); // version 0
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);

        let (decoded, ts) = decode_framed_with_hlc(&frame).unwrap();
        assert_eq!(ts, 0, "v0 frames should have hlc_ts = 0");
        assert_eq!(decoded.type_name(), "Pong");
    }

    #[test]
    fn type_tag_values_are_unique() {
        use crate::identity::NodeKeypair;
        let nid = NodeKeypair::generate().node_id();
        let cid = Cid::from_data(b"test");

        let variants: Vec<WireMessage> = vec![
            WireMessage::Ping { nonce: 0 },
            WireMessage::Pong { nonce: 0 },
            WireMessage::PieceRequest {
                cid,
                piece_indices: vec![],
                request_id: 0,
            },
            WireMessage::PieceResponse {
                pieces: vec![],
                request_id: 0,
            },
            WireMessage::ProviderAnnounce { cid, node_id: nid },
            WireMessage::SignedWrite {
                payload: vec![],
                signature: NodeKeypair::generate().sign(b"test"),
                writer: nid,
                cas_version: 0,
            },
            WireMessage::SwimJoin {
                node_id: nid,
                listen_port: 0,
            },
            WireMessage::SwimAlive {
                node_id: nid,
                incarnation: 0,
            },
            WireMessage::SwimSuspect {
                node_id: nid,
                incarnation: 0,
                from: nid,
            },
            WireMessage::SwimDead {
                node_id: nid,
                incarnation: 0,
                from: nid,
            },
            WireMessage::SwimPing {
                from: nid,
                nonce: 0,
                piggyback: vec![],
            },
            WireMessage::SwimPingAck {
                from: nid,
                nonce: 0,
                incarnation: 0,
            },
            WireMessage::HealthReport {
                cid,
                available_pieces: 0,
                target_pieces: 0,
            },
        ];

        let tags: Vec<u32> = variants.iter().map(|m| m.type_tag()).collect();
        let mut unique = tags.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(tags.len(), unique.len(), "all type tags must be unique");
    }
}
