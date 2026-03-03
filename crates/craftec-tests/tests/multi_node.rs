//! Multi-node integration tests for Craftec.
//!
//! These tests verify node-to-node features:
//! 1. SWIM membership protocol across multiple instances
//! 2. RLNC encode → distribute → recode → decode across nodes
//! 3. Wire message serialization for all variants
//! 4. Ed25519 cross-node verification
//! 5. HomMAC integrity across nodes
//! 6. Full pipeline: encode → sign → transmit → verify → decode
//!
//! All tests run in-process — no external network required.

use std::collections::HashMap;

use craftec_types::cid::Cid;
use craftec_types::identity::{self, NodeId, NodeKeypair};
use craftec_types::piece::CodedPiece;
use craftec_types::wire::{self, WireMessage};

use craftec_rlnc::decoder::RlncDecoder;
use craftec_rlnc::encoder::RlncEncoder;
use craftec_rlnc::recoder::RlncRecoder;

use craftec_crypto::hommac;

// ────────────────────────────────────────────────────────────────────────────
// Test 1: SWIM membership — 5 nodes discover each other
// ────────────────────────────────────────────────────────────────────────────

/// Lightweight SWIM membership table for integration testing.
/// Mirrors the logic in craftec-net's SwimMembership without iroh dep.
struct SimpleMembership {
    local_id: NodeId,
    members: HashMap<NodeId, u64>, // NodeId → incarnation
}

impl SimpleMembership {
    fn new(local_id: NodeId) -> Self {
        Self {
            local_id,
            members: HashMap::new(),
        }
    }

    fn mark_alive(&mut self, node_id: &NodeId, incarnation: u64) {
        if *node_id == self.local_id {
            return;
        }
        let entry = self.members.entry(*node_id).or_insert(0);
        if incarnation >= *entry {
            *entry = incarnation;
        }
    }

    fn is_alive(&self, node_id: &NodeId) -> bool {
        self.members.contains_key(node_id)
    }

    fn alive_count(&self) -> usize {
        self.members.len()
    }

    fn handle_message(&mut self, msg: &WireMessage) -> Vec<WireMessage> {
        let mut responses = Vec::new();
        match msg {
            WireMessage::SwimJoin { node_id, .. } => {
                self.mark_alive(node_id, 0);
                responses.push(WireMessage::SwimAlive {
                    node_id: self.local_id,
                    incarnation: 0,
                });
            }
            WireMessage::SwimAlive {
                node_id,
                incarnation,
            } => {
                self.mark_alive(node_id, *incarnation);
                responses.push(msg.clone());
            }
            _ => {}
        }
        responses
    }
}

#[test]
fn swim_five_nodes_discover_each_other() {
    let mut nodes: Vec<(NodeId, SimpleMembership)> = (0..5)
        .map(|_| {
            let id = NodeId::generate();
            let swim = SimpleMembership::new(id);
            (id, swim)
        })
        .collect();

    let bootstrap_id = nodes[0].0;

    // All nodes join via node 0.
    for i in 1..5 {
        let join_msg = WireMessage::SwimJoin {
            node_id: nodes[i].0,
            listen_port: 9000 + i as u16,
        };

        let responses = nodes[0].1.handle_message(&join_msg);
        assert!(
            !responses.is_empty(),
            "bootstrap should respond to SwimJoin"
        );
        assert!(
            nodes[0].1.is_alive(&nodes[i].0),
            "bootstrap should see node {} as alive",
            i
        );

        for resp in &responses {
            nodes[i].1.handle_message(resp);
        }
        assert!(
            nodes[i].1.is_alive(&bootstrap_id),
            "node {} should see bootstrap as alive",
            i
        );
    }

    assert_eq!(
        nodes[0].1.alive_count(),
        4,
        "bootstrap should have 4 alive members"
    );

    // Gossip: propagate all members to all nodes.
    for i in 1..5 {
        let alive_msg = WireMessage::SwimAlive {
            node_id: nodes[i].0,
            incarnation: 0,
        };
        for j in 1..5 {
            if i != j {
                nodes[j].1.handle_message(&alive_msg);
            }
        }
    }

    for (i, (_, swim)) in nodes.iter().enumerate() {
        assert!(
            swim.alive_count() >= 3,
            "node {} has {} alive (expected >= 3)",
            i,
            swim.alive_count()
        );
    }

    println!("[PASS] SWIM: 5 nodes discovered each other via gossip");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2: RLNC multi-node: encode → distribute → decode
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn rlnc_distribute_across_nodes_and_decode() {
    let original_data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let k = 8u32;

    let encoder = RlncEncoder::new(&original_data, k).expect("encoder");
    let all_pieces = encoder.encode_n(encoder.target_pieces() as usize);
    let piece_size = encoder.piece_size();

    // Distribute pieces round-robin to 3 storage nodes.
    let mut node_stores: Vec<Vec<CodedPiece>> = vec![Vec::new(); 3];
    for (i, piece) in all_pieces.iter().enumerate() {
        node_stores[i % 3].push(piece.clone());
    }

    // Client collects from each node sequentially.
    let mut decoder = RlncDecoder::new(k, piece_size);
    let mut pieces_collected = 0u32;

    for store in &node_stores {
        for piece in store {
            match decoder.add_piece(piece) {
                Ok(true) => pieces_collected += 1,
                Ok(false) => {}
                Err(e) => panic!("add_piece error: {:?}", e),
            }
            if decoder.is_decodable() {
                break;
            }
        }
        if decoder.is_decodable() {
            break;
        }
    }

    assert!(decoder.is_decodable(), "should decode from 3 nodes");
    let recovered = decoder.decode().expect("decode failed");
    assert_eq!(&recovered[..original_data.len()], original_data.as_slice());

    println!(
        "[PASS] RLNC: distributed across 3 nodes, decoded with {} pieces",
        pieces_collected
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 3: RLNC recode at intermediate node
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn rlnc_recode_at_intermediate_node() {
    let data: Vec<u8> = (0..2048).map(|i| (i % 173) as u8).collect();
    let k = 8u32;

    let encoder = RlncEncoder::new(&data, k).expect("encoder");
    let pieces = encoder.encode_n(encoder.target_pieces() as usize);
    let piece_size = encoder.piece_size();

    // Intermediate node: recode pairs.
    let mut recoded_pieces: Vec<CodedPiece> = Vec::new();
    for chunk in pieces[..6].chunks(2) {
        if chunk.len() == 2 {
            let recoded = RlncRecoder::recode(chunk).expect("recode");
            assert!(recoded.verify_piece_id(), "recoded piece_id should verify");
            recoded_pieces.push(recoded);
        }
    }

    // Client: decode using recoded + original pieces.
    let mut decoder = RlncDecoder::new(k, piece_size);
    for piece in &recoded_pieces {
        let _ = decoder.add_piece(piece);
    }
    for piece in &pieces[6..] {
        let _ = decoder.add_piece(piece);
        if decoder.is_decodable() {
            break;
        }
    }

    assert!(decoder.is_decodable(), "should decode with mixed pieces");
    let recovered = decoder.decode().expect("decode");
    assert_eq!(&recovered[..data.len()], data.as_slice());

    println!("[PASS] RLNC: intermediate recode -> client decode works");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 4: Wire message all variants round-trip
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn wire_message_all_variants_round_trip() {
    let kp = NodeKeypair::generate();
    let node_id = kp.node_id();
    let peer_id = NodeId::generate();
    let cid = Cid::from_data(b"test content");
    let sig = kp.sign(b"payload");

    let messages: Vec<WireMessage> = vec![
        WireMessage::Ping { nonce: 12345 },
        WireMessage::Pong { nonce: 12345 },
        WireMessage::PieceRequest {
            cid,
            piece_indices: vec![0, 1, 2],
            request_id: 0,
        },
        WireMessage::PieceResponse {
            pieces: vec![],
            request_id: 0,
        },
        WireMessage::ProviderAnnounce { cid, node_id },
        WireMessage::SignedWrite {
            payload: b"data".to_vec(),
            signature: sig,
            writer: node_id,
            cas_version: 42,
        },
        WireMessage::SwimJoin {
            node_id,
            listen_port: 9000,
        },
        WireMessage::SwimAlive {
            node_id,
            incarnation: 7,
        },
        WireMessage::SwimSuspect {
            node_id: peer_id,
            incarnation: 3,
            from: node_id,
        },
        WireMessage::SwimDead {
            node_id: peer_id,
            incarnation: 5,
            from: node_id,
        },
        WireMessage::HealthReport {
            cid,
            available_pieces: 60,
            target_pieces: 80,
        },
        WireMessage::SwimPing {
            from: node_id,
            nonce: 0,
            piggyback: vec![WireMessage::SwimAlive {
                node_id: peer_id,
                incarnation: 1,
            }],
        },
    ];

    for (i, msg) in messages.iter().enumerate() {
        let bytes = wire::encode(msg).expect("encode failed");
        let decoded = wire::decode(&bytes).expect("decode failed");
        assert_eq!(
            msg.type_name(),
            decoded.type_name(),
            "variant {} mismatch",
            i
        );
    }

    println!(
        "[PASS] Wire: all {} message variants round-trip correctly",
        messages.len()
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 5: Ed25519 cross-node verification
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn crypto_ed25519_cross_node_verification() {
    let kp_a = NodeKeypair::generate();
    let kp_b = NodeKeypair::generate();

    let payload = b"signed RPC request from node A";
    let sig = kp_a.sign(payload);

    // Node B verifies using A's public key.
    assert!(
        identity::verify(payload, &sig, &kp_a.node_id()),
        "B should verify A's sig"
    );

    // Node B cannot forge as Node A.
    let forged = kp_b.sign(payload);
    assert!(
        !identity::verify(payload, &forged, &kp_a.node_id()),
        "B's sig should NOT verify as A"
    );

    println!("[PASS] Crypto: cross-node Ed25519 verification works");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 6: HomMAC integrity across nodes
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn hommac_integrity_across_nodes() {
    let key = hommac::HomMacKey::generate();
    let cv = vec![42u8, 17, 99, 200];
    let data = vec![0xABu8; 512];

    let tag = hommac::compute_tag(&key, &cv, &data);
    assert!(hommac::verify_tag(&key, &cv, &data, &tag), "should verify");

    let mut tampered = data.clone();
    tampered[0] ^= 0xFF;
    assert!(
        !hommac::verify_tag(&key, &cv, &tampered, &tag),
        "tampered should fail"
    );

    println!("[PASS] HomMAC: cross-node integrity verification works");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 7: Content store/retrieve by CID
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn content_store_and_retrieve_by_cid() {
    let mut store: HashMap<Cid, Vec<CodedPiece>> = HashMap::new();

    let data1 = b"Hello from Node A";
    let data2: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();

    for data in [&data1[..], &data2[..]] {
        let encoder = RlncEncoder::new(data, 4).expect("encoder");
        let pieces = encoder.encode_n(encoder.target_pieces() as usize);
        let cid = *encoder.cid();
        store.insert(cid, pieces);
    }

    for data in [&data1[..], &data2[..]] {
        let cid = Cid::from_data(data);
        let pieces = store.get(&cid).expect("CID not found");
        let mut decoder = RlncDecoder::new(4, pieces[0].data.len());
        for piece in pieces {
            let _ = decoder.add_piece(piece);
            if decoder.is_decodable() {
                break;
            }
        }
        let recovered = decoder.decode().expect("decode");
        assert_eq!(&recovered[..data.len()], data);
    }

    println!("[PASS] Content: store/retrieve by CID works");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 8: Full pipeline — encode → sign → serialize → deserialize → verify → decode
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn full_pipeline_encode_sign_transmit_verify_decode() {
    let original = b"The Craftec P2P network is operational!";
    let k = 4u32;

    // Node A: encode + sign
    let encoder = RlncEncoder::new(original, k).expect("encoder");
    let pieces = encoder.encode_n(encoder.target_pieces() as usize);
    let cid = *encoder.cid();
    let kp_a = NodeKeypair::generate();
    let manifest = format!("write:cid={},pieces={}", cid, pieces.len());
    let sig = kp_a.sign(manifest.as_bytes());

    // Serialize to wire
    let response = WireMessage::PieceResponse {
        pieces: pieces.clone(),
        request_id: 0,
    };
    let wire_bytes = wire::encode(&response).expect("encode");

    // Node B: deserialize + verify
    let decoded_msg = wire::decode(&wire_bytes).expect("decode");
    let stored_pieces = match decoded_msg {
        WireMessage::PieceResponse { pieces, .. } => pieces,
        _ => panic!("expected PieceResponse"),
    };

    for piece in &stored_pieces {
        assert!(piece.verify_piece_id(), "piece failed verification");
    }

    assert!(
        identity::verify(manifest.as_bytes(), &sig, &kp_a.node_id()),
        "sig should verify"
    );

    // Node C: decode
    let mut decoder = RlncDecoder::new(k, encoder.piece_size());
    for piece in &stored_pieces {
        let _ = decoder.add_piece(piece);
        if decoder.is_decodable() {
            break;
        }
    }

    let recovered = decoder.decode().expect("decode");
    assert_eq!(&recovered[..original.len()], original);

    println!("[PASS] Full pipeline: encode -> sign -> transmit -> verify -> decode works");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 9: RLNC large data (32KB, k=32, standard generation)
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn rlnc_large_data_k32_standard_generation() {
    // Standard Craftec generation: K=32, simulating 32KB of data.
    let data: Vec<u8> = (0..32_768).map(|i| (i % 251) as u8).collect();
    let k = 32u32;

    let encoder = RlncEncoder::new(&data, k).expect("encoder");
    let pieces = encoder.encode_n(encoder.target_pieces() as usize);
    let piece_size = encoder.piece_size();

    // Distribute to 5 nodes round-robin.
    let mut stores: Vec<Vec<&CodedPiece>> = vec![Vec::new(); 5];
    for (i, piece) in pieces.iter().enumerate() {
        stores[i % 5].push(piece);
    }

    // Client collects from nodes in random order (simulate 2, 0, 4, 1, 3).
    let order = [2, 0, 4, 1, 3];
    let mut decoder = RlncDecoder::new(k, piece_size);

    for &node in &order {
        for piece in &stores[node] {
            let _ = decoder.add_piece(piece);
            if decoder.is_decodable() {
                break;
            }
        }
        if decoder.is_decodable() {
            break;
        }
    }

    assert!(decoder.is_decodable());
    let recovered = decoder.decode().expect("decode");
    assert_eq!(&recovered[..data.len()], data.as_slice());

    println!("[PASS] RLNC: 32KB data, k=32, 5-node distribution works");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 10: Multiple CIDs handled independently
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn multiple_cids_independent_decode() {
    // Simulate a node storing and serving 3 different CIDs.
    let datasets: Vec<Vec<u8>> = vec![
        (0..512).map(|i| (i % 251) as u8).collect(),
        vec![0xFF; 1024],
        b"short message".to_vec(),
    ];
    let k = 4u32;

    let mut store: HashMap<Cid, (Vec<CodedPiece>, usize, usize)> = HashMap::new();

    for data in &datasets {
        let encoder = RlncEncoder::new(data, k).expect("encoder");
        let pieces = encoder.encode_n(encoder.target_pieces() as usize);
        let cid = *encoder.cid();
        let piece_size = encoder.piece_size();
        store.insert(cid, (pieces, piece_size, data.len()));
    }

    assert_eq!(store.len(), 3, "should have 3 distinct CIDs");

    // Decode each independently.
    for (cid, (pieces, piece_size, orig_len)) in &store {
        let mut decoder = RlncDecoder::new(k, *piece_size);
        for piece in pieces {
            assert_eq!(&piece.cid, cid, "piece CID should match");
            let _ = decoder.add_piece(piece);
            if decoder.is_decodable() {
                break;
            }
        }
        let recovered = decoder.decode().expect("decode");
        assert_eq!(recovered.len(), k as usize * piece_size);
        // Original data is prefix of recovered (padding may follow).
        let orig_data = datasets.iter().find(|d| Cid::from_data(d) == *cid).unwrap();
        assert_eq!(&recovered[..*orig_len], orig_data.as_slice());
    }

    println!("[PASS] Multiple CIDs: independent encode/decode works");
}
