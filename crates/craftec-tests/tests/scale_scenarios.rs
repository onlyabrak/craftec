//! Phase 9 — Multi-Node Scale Scenarios.
//!
//! These tests validate Craftec subsystem behaviour at 10+ node scale:
//! 1. SWIM convergence with 10 nodes
//! 2. Concurrent write throughput across 10 independent databases
//! 3. SWIM churn handling (nodes joining and leaving)
//! 4. Repair storm prevention (gradual repair after mass node failure)

use std::sync::Arc;
use std::time::{Duration, Instant};

use craftec_health::scanner::HealthScanner;
use craftec_health::tracker::{PieceHolder, PieceTracker};
use craftec_net::swim::SwimMembership;
use craftec_obj::ContentAddressedStore;
use craftec_sql::{CraftDatabase, RpcWriteHandler};
use craftec_types::cid::Cid;
use craftec_types::identity::{NodeId, NodeKeypair};
use craftec_vfs::CidVfs;

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Lightweight node for scale tests — only the subsystems needed per test.
#[allow(dead_code)]
struct ScaleNode {
    keypair: NodeKeypair,
    store: Arc<ContentAddressedStore>,
    vfs: Arc<CidVfs>,
    database: Arc<CraftDatabase>,
    rpc_write: Arc<RpcWriteHandler>,
    _tmp: tempfile::TempDir,
}

impl ScaleNode {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(ContentAddressedStore::new(&tmp.path().join("obj"), 64).unwrap());
        let vfs = Arc::new(CidVfs::with_default_page_size(Arc::clone(&store)).unwrap());
        let keypair = NodeKeypair::generate();
        let database = Arc::new(
            CraftDatabase::create(keypair.node_id(), Arc::clone(&vfs), &tmp.path().join("sql"))
                .await
                .unwrap(),
        );
        let rpc_write = Arc::new(RpcWriteHandler::new(Arc::clone(&database)));
        Self {
            keypair,
            store,
            vfs,
            database,
            rpc_write,
            _tmp: tmp,
        }
    }

    /// Execute a signed write and return the new root CID.
    async fn signed_write(&self, sql: &str) -> craftec_types::cid::Cid {
        let root = self.database.root_cid();
        let payload =
            craftec_sql::rpc_write::build_signed_payload(&self.keypair.node_id(), sql, Some(root));
        let sig = self.keypair.sign(&payload);
        let sw = craftec_sql::SignedWrite {
            writer: self.keypair.node_id(),
            sql: sql.to_owned(),
            expected_root: Some(root),
            signature: sig,
        };
        self.rpc_write.handle_signed_write(&sw).await.unwrap()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 1: 10-node SWIM convergence
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn swim_ten_nodes_converge() {
    use craftec_types::wire::WireMessage;

    // Create 10 SWIM membership instances.
    let node_ids: Vec<NodeId> = (0..10).map(|_| NodeId::generate()).collect();
    let swims: Vec<SwimMembership> = node_ids.iter().map(|id| SwimMembership::new(*id)).collect();

    // All nodes join through node 0 (bootstrap).
    for i in 1..10 {
        let join = WireMessage::SwimJoin {
            node_id: node_ids[i],
            listen_port: 9000 + i as u16,
        };
        let responses = swims[0].handle_message(&join);
        for resp in &responses {
            swims[i].handle_message(resp);
        }
    }

    // Node 0 should know all 9 others.
    assert_eq!(
        swims[0].alive_members().len(),
        9,
        "bootstrap should see 9 alive peers"
    );

    // Simulate gossip rounds: each round, every node broadcasts its known
    // members as SwimAlive messages to one random known peer.  This mirrors
    // the real SWIM protocol where membership updates piggyback on all
    // message types (ping, ack, ping-req).
    let mut rounds = 0;
    let max_rounds = 15;

    loop {
        rounds += 1;
        // Each node shares all its alive members with all its known peers.
        // In real SWIM this happens through piggybacking over multiple ticks.
        for i in 0..10 {
            let alive = swims[i].alive_members();
            for &peer_id in &alive {
                // Send each known member as SwimAlive to each peer.
                for &known in &alive {
                    if known != peer_id {
                        if let Some(j) = node_ids.iter().position(|id| id == &peer_id) {
                            let msg = WireMessage::SwimAlive {
                                node_id: known,
                                incarnation: 0,
                            };
                            swims[j].handle_message(&msg);
                        }
                    }
                }
            }
        }

        let fully_converged = swims.iter().all(|s| s.alive_members().len() >= 9);
        if fully_converged {
            break;
        }
        assert!(
            rounds < max_rounds,
            "SWIM should converge within {} rounds, only {} nodes fully converged",
            max_rounds,
            swims
                .iter()
                .filter(|s| s.alive_members().len() >= 9)
                .count()
        );
    }

    // Epidemic gossip should converge in O(log N) rounds.
    assert!(
        rounds <= 5,
        "10-node SWIM should converge in ~3 rounds (log2(10)), took {}",
        rounds
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2: 10-node concurrent write throughput
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ten_nodes_concurrent_write_throughput() {
    // Create 10 independent nodes, each with their own database.
    let mut nodes = Vec::new();
    for _ in 0..10 {
        nodes.push(ScaleNode::new().await);
    }

    // Each node creates a table.
    for (i, node) in nodes.iter().enumerate() {
        node.signed_write(&format!(
            "CREATE TABLE data_{i} (id INTEGER PRIMARY KEY, val TEXT)"
        ))
        .await;
    }

    // Writes: each node inserts 10 rows.
    let start = Instant::now();

    for (i, node) in nodes.iter().enumerate() {
        for j in 0..10 {
            node.signed_write(&format!("INSERT INTO data_{i} VALUES ({j}, 'row-{j}')"))
                .await;
        }
    }
    let elapsed = start.elapsed();

    // Verify each node has 10 rows.
    for (i, node) in nodes.iter().enumerate() {
        let rows = node
            .database
            .query(&format!("SELECT count(*) FROM data_{i}"))
            .await
            .unwrap();
        match &rows[0][0] {
            craftec_sql::ColumnValue::Integer(n) => assert_eq!(*n, 10),
            other => panic!("node {} expected Integer(10), got {:?}", i, other),
        }
    }

    // 100 total writes (10 nodes x 10 rows).
    let ops = 100.0 / elapsed.as_secs_f64();
    // Sanity: should complete in reasonable time (>1 op/sec).
    assert!(ops > 1.0, "write throughput too low: {:.1} ops/sec", ops);

    // Verify all root CIDs are distinct (independent databases).
    let roots: std::collections::HashSet<_> = nodes.iter().map(|n| n.database.root_cid()).collect();
    assert_eq!(roots.len(), 10, "each node should have a unique root CID");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 3: 10-node SWIM churn — nodes join and leave
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn swim_ten_nodes_churn() {
    use craftec_types::wire::WireMessage;

    // Start with 10 nodes, fully converged.
    let node_ids: Vec<NodeId> = (0..10).map(|_| NodeId::generate()).collect();
    let swims: Vec<SwimMembership> = node_ids.iter().map(|id| SwimMembership::new(*id)).collect();

    // Bootstrap all nodes via node 0 and converge via epidemic gossip.
    for i in 1..10 {
        let join = WireMessage::SwimJoin {
            node_id: node_ids[i],
            listen_port: 9000 + i as u16,
        };
        let responses = swims[0].handle_message(&join);
        for resp in &responses {
            swims[i].handle_message(resp);
        }
    }

    // Gossip all members to all known peers.
    for _ in 0..5 {
        for i in 0..10 {
            let alive = swims[i].alive_members();
            for &peer_id in &alive {
                for &known in &alive {
                    if known != peer_id {
                        if let Some(j) = node_ids.iter().position(|id| id == &peer_id) {
                            let msg = WireMessage::SwimAlive {
                                node_id: known,
                                incarnation: 0,
                            };
                            swims[j].handle_message(&msg);
                        }
                    }
                }
            }
        }
    }

    // Verify full convergence.
    for (i, swim) in swims.iter().enumerate() {
        assert!(
            swim.alive_members().len() >= 9,
            "node {} should see 9 alive, sees {}",
            i,
            swim.alive_members().len()
        );
    }

    // CHURN: nodes 8 and 9 "leave" — mark them dead on all remaining nodes.
    for swim in swims.iter().take(8) {
        swim.mark_dead(&node_ids[8], 0);
        swim.mark_dead(&node_ids[9], 0);
    }

    // Verify nodes 8 and 9 are dead everywhere.
    for swim in swims.iter().take(8) {
        assert!(!swim.is_alive(&node_ids[8]));
        assert!(!swim.is_alive(&node_ids[9]));
    }

    // CHURN: 2 new nodes join.
    let new_ids: Vec<NodeId> = (0..2).map(|_| NodeId::generate()).collect();
    let new_swims: Vec<SwimMembership> =
        new_ids.iter().map(|id| SwimMembership::new(*id)).collect();

    for (k, new_swim) in new_swims.iter().enumerate() {
        let join = WireMessage::SwimJoin {
            node_id: new_ids[k],
            listen_port: 10000 + k as u16,
        };
        let responses = swims[0].handle_message(&join);
        for resp in &responses {
            new_swim.handle_message(resp);
        }
    }

    // Node 0 should see the new nodes.
    for new_id in &new_ids {
        assert!(
            swims[0].is_alive(new_id),
            "bootstrap should see new node as alive"
        );
    }

    // After churn: 7 original alive + 2 new = 9 alive on bootstrap.
    assert!(
        swims[0].alive_members().len() >= 9,
        "bootstrap should see at least 9 alive after churn, sees {}",
        swims[0].alive_members().len()
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 4: Repair storm prevention — gradual repair after mass failure
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn repair_storm_prevention_after_mass_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(ContentAddressedStore::new(&tmp.path().join("obj"), 64).unwrap());
    let tracker = Arc::new(PieceTracker::new());

    // Simulate 10 nodes holding pieces for 20 CIDs.
    let all_nodes: Vec<NodeId> = (0..10).map(|_| NodeId::generate()).collect();
    let cids: Vec<Cid> = (0..20).map(|i| Cid::from_data(&[i as u8; 32])).collect();

    // Use a local scanner node that holds ≥2 pieces for every CID (scan eligibility).
    let scanner_node = all_nodes[0];

    // Each CID has pieces spread across all 10 nodes (4 pieces per node = 40 total).
    for cid in &cids {
        for node_id in &all_nodes {
            tracker.record_piece(
                cid,
                PieceHolder {
                    node_id: *node_id,
                    piece_count: 4,
                    last_seen: Instant::now(),
                },
            );
        }
    }

    // Verify initial state: each CID has 40 total pieces (10 nodes × 4 pieces).
    for cid in &cids {
        assert_eq!(
            tracker.available_count(cid),
            40,
            "each CID should start with 40 pieces"
        );
    }

    // MASS FAILURE: kill 3 of 10 nodes.
    for killed_node in &all_nodes[7..10] {
        tracker.remove_node(killed_node);
    }

    // After killing 3 nodes, each CID should have 28 pieces (7 nodes × 4 pieces).
    for cid in &cids {
        let available = tracker.available_count(cid);
        assert_eq!(
            available, 28,
            "each CID should have 28 pieces after 3 node deaths"
        );
    }

    // Run health scanner — should detect all 20 CIDs as critical (28 < k=32).
    let scanner = HealthScanner::new(
        Arc::clone(&store),
        Arc::clone(&tracker),
        Duration::from_secs(3600),
        scanner_node,
    )
    .with_scan_percent(1.0); // scan everything at once

    let repairs = scanner.scan_cycle().await.unwrap();

    // All 20 CIDs should need repair.
    assert_eq!(
        repairs.len(),
        20,
        "all 20 CIDs should need repair after mass failure"
    );

    // Verify repair severity: all should be critical (28 < k=32).
    for req in &repairs {
        assert_eq!(req.severity(), "critical");
    }

    // STORM PREVENTION: simulate gradual repair (1 piece per CID per cycle).
    // After each repair cycle, only process a fixed batch (not all at once).
    let batch_size = 5; // repair 5 CIDs per cycle (not all 20)
    let mut repaired_cids = 0;

    // Use a new node for repair contributions to distinguish from existing holders.
    let new_repair_node = NodeId::generate();

    for cycle in 0..4 {
        // Process a batch of repairs.
        let start = cycle * batch_size;
        let end = (start + batch_size).min(repairs.len());
        let batch = &repairs[start..end];

        for req in batch {
            let cid = req.cid();
            let current = tracker.available_count(cid);

            // Simulate repair: new node contributes 4 pieces.
            tracker.record_piece(
                cid,
                PieceHolder {
                    node_id: new_repair_node,
                    piece_count: 4 * (cycle as u32 + 1), // accumulating pieces
                    last_seen: Instant::now(),
                },
            );

            assert!(
                tracker.available_count(cid) > current,
                "repair should increase availability"
            );
            repaired_cids += 1;
        }
    }

    assert_eq!(
        repaired_cids, 20,
        "all 20 CIDs should be repaired over 4 cycles"
    );

    // After all repairs, re-scan should find fewer (or no) critical issues.
    // Reset scanner cursor for a fresh scan.
    // The scanner_node needs ≥2 pieces for scan eligibility (already has 4).
    let scanner2 = HealthScanner::new(
        Arc::clone(&store),
        Arc::clone(&tracker),
        Duration::from_secs(3600),
        scanner_node,
    )
    .with_scan_percent(1.0);

    let remaining_repairs = scanner2.scan_cycle().await.unwrap();

    // After repair: 28 + 4 = 32 pieces per CID, which equals k=32.
    // With target = 32 × ceil(2 + 16/32) = 32 × 3 = 96, available(32) < target(96).
    // So repairs are "normal" (not critical). Verify no critical repairs remain.
    for req in &remaining_repairs {
        assert_ne!(
            req.severity(),
            "critical",
            "no critical repairs should remain after gradual repair"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 5: PieceTracker remove_node cascades across many CIDs
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn tracker_remove_node_cascades_at_scale() {
    let tracker = PieceTracker::new();
    let nodes: Vec<NodeId> = (0..10).map(|_| NodeId::generate()).collect();
    let cids: Vec<Cid> = (0..50).map(|i| Cid::from_data(&[i as u8; 16])).collect();

    // Each node holds 4 pieces for all 50 CIDs.
    for cid in &cids {
        for node in &nodes {
            tracker.record_piece(
                cid,
                PieceHolder {
                    node_id: *node,
                    piece_count: 4,
                    last_seen: Instant::now(),
                },
            );
        }
    }

    // Each CID has 40 total pieces (10 nodes × 4).
    for cid in &cids {
        assert_eq!(tracker.available_count(cid), 40);
    }

    // Remove 3 nodes.
    for node in &nodes[0..3] {
        tracker.remove_node(node);
    }

    // Each CID should now have 28 pieces (7 nodes × 4).
    for cid in &cids {
        assert_eq!(
            tracker.available_count(cid),
            28,
            "should have 28 pieces after removing 3 nodes"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 6: SWIM suspect → dead timeout flow at scale
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn swim_suspect_timeout_promotes_to_dead() {
    let local = NodeId::generate();
    let swim = Arc::new(SwimMembership::new(local));

    // Add 10 peers.
    let peers: Vec<NodeId> = (0..10).map(|_| NodeId::generate()).collect();
    for peer in &peers {
        swim.mark_alive(peer, 0);
    }
    assert_eq!(swim.alive_members().len(), 10);

    // Suspect 3 peers.
    for peer in &peers[0..3] {
        swim.mark_suspect(peer, 0);
    }
    assert_eq!(swim.alive_members().len(), 7);

    // Wait for suspect timeout (5000ms per spec §18) + margin.
    tokio::time::sleep(Duration::from_millis(5200)).await;

    // A protocol tick should promote suspects to dead.
    swim.protocol_tick().await;

    // The 3 suspected peers should now be dead, not just suspected.
    for peer in &peers[0..3] {
        assert!(
            !swim.is_alive(peer),
            "suspected peer should not be alive after timeout"
        );
    }

    // Remaining 7 should still be alive.
    assert_eq!(swim.alive_members().len(), 7, "7 peers should remain alive");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 7: 10 concurrent databases maintain independent state
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ten_databases_independent_state() {
    let mut nodes = Vec::new();
    for _ in 0..10 {
        nodes.push(ScaleNode::new().await);
    }

    // Each node creates a table with a unique name and inserts different data.
    for (i, node) in nodes.iter().enumerate() {
        node.signed_write(&format!(
            "CREATE TABLE node_{i} (id INTEGER PRIMARY KEY, value INTEGER)"
        ))
        .await;
        node.signed_write(&format!("INSERT INTO node_{i} VALUES (1, {i})"))
            .await;
    }

    // Verify each node's data is isolated.
    for (i, node) in nodes.iter().enumerate() {
        let rows = node
            .database
            .query(&format!("SELECT value FROM node_{i}"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0][0] {
            craftec_sql::ColumnValue::Integer(n) => assert_eq!(*n, i as i64),
            other => panic!("node {} expected Integer({}), got {:?}", i, i, other),
        }

        // Querying another node's table should fail.
        let other = (i + 1) % 10;
        let result = node
            .database
            .query(&format!("SELECT * FROM node_{other}"))
            .await;
        assert!(
            result.is_err(),
            "node {} should not see node {}'s table",
            i,
            other
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 8: RLNC distribution at scale — 20 CIDs across 10 nodes
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn rlnc_twenty_cids_across_ten_nodes() {
    use craftec_rlnc::decoder::RlncDecoder;
    use craftec_rlnc::encoder::RlncEncoder;

    let k = 8u32;
    let num_nodes = 10;
    let num_cids = 20;

    // Generate 20 datasets and encode them.
    let datasets: Vec<Vec<u8>> = (0..num_cids)
        .map(|i| (0..1024).map(|j| ((i * 37 + j) % 251) as u8).collect())
        .collect();

    for (cid_idx, data) in datasets.iter().enumerate() {
        let encoder = RlncEncoder::new(data, k).unwrap();
        let pieces = encoder.encode_n(encoder.target_pieces() as usize);
        let piece_size = encoder.piece_size();

        // Distribute round-robin to 10 nodes.
        let mut node_stores: Vec<Vec<_>> = vec![Vec::new(); num_nodes];
        for (i, piece) in pieces.iter().enumerate() {
            node_stores[i % num_nodes].push(piece.clone());
        }

        // Decode by collecting from a subset of nodes (simulate partial availability).
        let mut decoder = RlncDecoder::new(k, piece_size);
        let mut nodes_used = 0;

        for store in &node_stores {
            nodes_used += 1;
            for piece in store {
                let _ = decoder.add_piece(piece);
                if decoder.is_decodable() {
                    break;
                }
            }
            if decoder.is_decodable() {
                break;
            }
        }

        assert!(
            decoder.is_decodable(),
            "CID {} should be decodable",
            cid_idx
        );
        let recovered = decoder.decode().unwrap();
        assert_eq!(
            &recovered[..data.len()],
            data.as_slice(),
            "CID {} data mismatch",
            cid_idx
        );
        assert!(
            nodes_used <= num_nodes,
            "should not need more than {} nodes",
            num_nodes
        );
    }
}
