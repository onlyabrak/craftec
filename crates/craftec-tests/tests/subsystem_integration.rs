//! Subsystem integration tests for Craftec.
//!
//! These tests exercise the full wiring of Craftec subsystems end-to-end,
//! verifying that the handler, database, store, health, and COM layers
//! work together correctly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use craftec_crypto::sign::KeyStore;
use craftec_health::tracker::{PieceHolder, PieceTracker};
use craftec_net::dht::DhtProviders;
use craftec_net::pending::PendingFetches;
use craftec_obj::ContentAddressedStore;
use craftec_rlnc::encoder::RlncEncoder;
use craftec_sql::{CraftDatabase, RpcWriteHandler};
use craftec_types::cid::Cid;
use craftec_types::identity::{NodeId, NodeKeypair};
use craftec_types::piece::CodedPiece;
use craftec_vfs::CidVfs;

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct TestNode {
    store: Arc<ContentAddressedStore>,
    vfs: Arc<CidVfs>,
    database: Arc<CraftDatabase>,
    rpc_write: Arc<RpcWriteHandler>,
    tracker: Arc<PieceTracker>,
    dht: Arc<DhtProviders>,
    pending: Arc<PendingFetches>,
    keypair: NodeKeypair,
    _tmp: tempfile::TempDir,
}

impl TestNode {
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
        let tracker = Arc::new(PieceTracker::new());
        let dht = Arc::new(DhtProviders::new());
        let pending = Arc::new(PendingFetches::new());
        Self {
            store,
            vfs,
            database,
            rpc_write,
            tracker,
            dht,
            pending,
            keypair,
            _tmp: tmp,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 1: RPC signed write end-to-end
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rpc_signed_write_end_to_end() {
    let node = TestNode::new().await;
    let root_before = node.database.root_cid();

    // Create a signed write message.
    let sql = "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)";
    let msg = craftec_sql::rpc_write::build_signed_payload(
        &node.keypair.node_id(),
        sql,
        Some(root_before),
    );
    let sig = node.keypair.sign(&msg);

    let signed_write = craftec_sql::SignedWrite {
        writer: node.keypair.node_id(),
        sql: sql.to_owned(),
        expected_root: Some(root_before),
        signature: sig,
    };

    // Execute the write.
    let new_root = node
        .rpc_write
        .handle_signed_write(&signed_write)
        .await
        .unwrap();
    assert_ne!(new_root, root_before, "root CID should change after write");

    // Verify we can query the new table.
    let rows = node
        .database
        .query("SELECT count(*) FROM users")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "query should return one row");
    match &rows[0][0] {
        craftec_sql::ColumnValue::Integer(n) => assert_eq!(*n, 0),
        other => panic!("expected Integer(0), got {:?}", other),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2: CAS conflict detection across full stack
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cas_conflict_on_stale_root() {
    let node = TestNode::new().await;

    // First write: create table.
    let root0 = node.database.root_cid();
    let sql1 = "CREATE TABLE items (id INTEGER PRIMARY KEY)";
    let msg1 =
        craftec_sql::rpc_write::build_signed_payload(&node.keypair.node_id(), sql1, Some(root0));
    let sig1 = node.keypair.sign(&msg1);
    let sw1 = craftec_sql::SignedWrite {
        writer: node.keypair.node_id(),
        sql: sql1.to_owned(),
        expected_root: Some(root0),
        signature: sig1,
    };
    let root1 = node.rpc_write.handle_signed_write(&sw1).await.unwrap();
    assert_ne!(root1, root0);

    // Second write with stale root (root0 instead of root1): should fail.
    let sql2 = "INSERT INTO items VALUES (1)";
    let msg2 = craftec_sql::rpc_write::build_signed_payload(
        &node.keypair.node_id(),
        sql2,
        Some(root0), // STALE
    );
    let sig2 = node.keypair.sign(&msg2);
    let sw2 = craftec_sql::SignedWrite {
        writer: node.keypair.node_id(),
        sql: sql2.to_owned(),
        expected_root: Some(root0),
        signature: sig2,
    };
    let result = node.rpc_write.handle_signed_write(&sw2).await;
    assert!(
        matches!(result, Err(craftec_sql::SqlError::CasConflict { .. })),
        "should get CAS conflict, got: {:?}",
        result
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 3: Non-owner write rejection
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn non_owner_write_rejected() {
    let node = TestNode::new().await;
    let attacker = NodeKeypair::generate();

    let root = node.database.root_cid();
    let sql = "CREATE TABLE evil (x INTEGER)";
    let msg = craftec_sql::rpc_write::build_signed_payload(&attacker.node_id(), sql, Some(root));
    let sig = attacker.sign(&msg);
    let sw = craftec_sql::SignedWrite {
        writer: attacker.node_id(),
        sql: sql.to_owned(),
        expected_root: Some(root),
        signature: sig,
    };
    let result = node.rpc_write.handle_signed_write(&sw).await;
    assert!(
        matches!(
            result,
            Err(craftec_sql::SqlError::UnauthorizedWriter { .. })
        ),
        "non-owner write should be rejected, got: {:?}",
        result
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 4: Store + retrieve content objects
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn store_put_get_roundtrip() {
    let node = TestNode::new().await;

    let data = b"Hello from Craftec integration test!";
    let cid = node.store.put(data).await.unwrap();

    let retrieved = node.store.get(&cid).await.unwrap();
    assert!(retrieved.is_some(), "data should exist");
    assert_eq!(retrieved.unwrap().as_ref(), data);
}

// ────────────────────────────────────────────────────────────────────────────
// Test 5: DHT provider tracking
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dht_provider_announce_and_query() {
    let node = TestNode::new().await;
    let cid = Cid::from_data(b"content-a");
    let provider1 = NodeId::generate();
    let provider2 = NodeId::generate();

    node.dht.announce_provider(&cid, &provider1);
    node.dht.announce_provider(&cid, &provider2);

    let providers = node.dht.get_providers(&cid);
    assert_eq!(providers.len(), 2);
    assert!(providers.contains(&provider1));
    assert!(providers.contains(&provider2));

    // Remove one provider.
    node.dht.remove_node(&provider1);
    let remaining = node.dht.get_providers(&cid);
    assert_eq!(remaining.len(), 1);
    assert!(remaining.contains(&provider2));
}

// ────────────────────────────────────────────────────────────────────────────
// Test 6: PieceTracker + HealthScanner integration
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_scanner_detects_under_replicated_cid() {
    let node = TestNode::new().await;
    let cid = Cid::from_data(b"under-replicated");

    // Record only 1 piece for this CID (well below k=32).
    node.tracker.record_piece(
        &cid,
        PieceHolder {
            node_id: NodeId::generate(),
            piece_index: 0,
            last_seen: Instant::now(),
        },
    );

    // Create a scanner and run a cycle.
    let scanner = craftec_health::scanner::HealthScanner::new(
        Arc::clone(&node.store),
        Arc::clone(&node.tracker),
        Duration::from_secs(3600),
    )
    .with_scan_percent(1.0); // scan everything in one cycle

    let repairs = scanner.scan_cycle().await.unwrap();
    assert!(
        !repairs.is_empty(),
        "scanner should detect under-replication"
    );

    // The repair should be critical (available < k=32).
    for req in &repairs {
        assert_eq!(req.severity(), "critical");
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 7: PendingFetches register/resolve flow
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pending_fetches_roundtrip() {
    let pending = PendingFetches::new();
    let cid = Cid::from_data(b"fetching-this");

    let rx = pending.register(&cid);

    // Simulate receiving a piece from the network.
    let piece = CodedPiece::new(cid, vec![1], vec![0xAB; 256], [0u8; 32]);
    pending.resolve(&cid, piece.clone());

    let received = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .expect("should not timeout")
        .expect("channel should not be dropped");

    assert_eq!(received.cid, cid);
    assert_eq!(received.data, vec![0xAB; 256]);
}

// ────────────────────────────────────────────────────────────────────────────
// Test 8: SQL write + query roundtrip through database
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sql_full_write_query_roundtrip() {
    let node = TestNode::new().await;
    let owner = node.keypair.node_id();

    // Create table.
    node.database
        .execute("CREATE TABLE logs (ts INTEGER, msg TEXT)", &owner)
        .await
        .unwrap();

    // Insert rows.
    node.database
        .execute("INSERT INTO logs VALUES (1, 'hello')", &owner)
        .await
        .unwrap();
    node.database
        .execute("INSERT INTO logs VALUES (2, 'world')", &owner)
        .await
        .unwrap();

    // Query.
    let rows = node
        .database
        .query("SELECT msg FROM logs ORDER BY ts")
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], craftec_sql::ColumnValue::Text("hello".into()));
    assert_eq!(rows[1][0], craftec_sql::ColumnValue::Text("world".into()));
}

// ────────────────────────────────────────────────────────────────────────────
// Test 9: CraftOBJ store + list_cids
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn store_list_cids_after_multiple_puts() {
    let node = TestNode::new().await;

    let data1 = b"object-one";
    let data2 = b"object-two";
    let data3 = b"object-three";

    let cid1 = node.store.put(data1).await.unwrap();
    let cid2 = node.store.put(data2).await.unwrap();
    let cid3 = node.store.put(data3).await.unwrap();

    let cids = node.store.list_cids().await.unwrap();
    assert!(cids.contains(&cid1));
    assert!(cids.contains(&cid2));
    assert!(cids.contains(&cid3));
    assert!(cids.len() >= 3);
}

// ────────────────────────────────────────────────────────────────────────────
// Test 10: CraftCOM runtime executes WASM with HostState
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn com_runtime_execute_with_host_state() {
    let node = TestNode::new().await;
    let keystore = Arc::new(KeyStore::new(node._tmp.path()).unwrap());
    let runtime = craftec_com::ComRuntime::new(100_000).unwrap();

    let host_state = craftec_com::HostState::new(
        Arc::clone(&node.store),
        Some(Arc::clone(&node.database)),
        keystore,
    );

    // Minimal WASM: (module (func (export "main") (result i32) i32.const 42))
    let wasm =
        wat::parse_str(r#"(module (func (export "main") (result i32) i32.const 42))"#).unwrap();

    let result = runtime
        .execute_agent(&wasm, "main", &[], host_state)
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].unwrap_i32(), 42);
}

// ────────────────────────────────────────────────────────────────────────────
// Test 11: RLNC encode → store pieces → retrieve → decode
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rlnc_store_retrieve_decode_roundtrip() {
    let node = TestNode::new().await;
    let original = b"Data to encode, store, retrieve, and decode";
    let k = 4u32;

    let encoder = RlncEncoder::new(original, k).unwrap();
    let pieces = encoder.encode_n(encoder.target_pieces() as usize);
    let piece_size = encoder.piece_size();
    let cid = *encoder.cid();

    // Store each piece's data in CraftOBJ.
    let mut piece_cids = Vec::new();
    for piece in &pieces {
        let stored_cid = node
            .store
            .put(&postcard::to_allocvec(piece).unwrap())
            .await
            .unwrap();
        piece_cids.push(stored_cid);
    }

    // Retrieve and decode.
    let mut decoder = craftec_rlnc::decoder::RlncDecoder::new(k, piece_size);
    for stored_cid in &piece_cids {
        let bytes = node.store.get(stored_cid).await.unwrap().unwrap();
        let piece: CodedPiece = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(piece.cid, cid);
        let _ = decoder.add_piece(&piece);
        if decoder.is_decodable() {
            break;
        }
    }

    assert!(decoder.is_decodable());
    let recovered = decoder.decode().unwrap();
    assert_eq!(&recovered[..original.len()], original);
}

// ────────────────────────────────────────────────────────────────────────────
// Test 12: VFS snapshot isolation
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vfs_snapshot_isolation_with_database() {
    let node = TestNode::new().await;
    let owner = node.keypair.node_id();

    // Create table and insert initial data.
    node.database
        .execute("CREATE TABLE kv (k TEXT, v TEXT)", &owner)
        .await
        .unwrap();
    node.database
        .execute("INSERT INTO kv VALUES ('a', '1')", &owner)
        .await
        .unwrap();

    let root_after_first = node.database.root_cid();

    // Insert more data.
    node.database
        .execute("INSERT INTO kv VALUES ('b', '2')", &owner)
        .await
        .unwrap();

    let root_after_second = node.database.root_cid();
    assert_ne!(root_after_first, root_after_second);

    // Query current state: should see both rows.
    let rows = node
        .database
        .query("SELECT * FROM kv ORDER BY k")
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
}

// ────────────────────────────────────────────────────────────────────────────
// Test 13: Multiple sequential writes maintain consistency
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn multiple_sequential_rpc_writes() {
    let node = TestNode::new().await;

    // Create table.
    let root0 = node.database.root_cid();
    let sql0 = "CREATE TABLE counters (id INTEGER PRIMARY KEY, val INTEGER)";
    let payload0 =
        craftec_sql::rpc_write::build_signed_payload(&node.keypair.node_id(), sql0, Some(root0));
    let sw0 = craftec_sql::SignedWrite {
        writer: node.keypair.node_id(),
        sql: sql0.to_owned(),
        expected_root: Some(root0),
        signature: node.keypair.sign(&payload0),
    };
    let root1 = node.rpc_write.handle_signed_write(&sw0).await.unwrap();

    // Insert row 1.
    let sql1 = "INSERT INTO counters VALUES (1, 100)";
    let payload1 =
        craftec_sql::rpc_write::build_signed_payload(&node.keypair.node_id(), sql1, Some(root1));
    let sw1 = craftec_sql::SignedWrite {
        writer: node.keypair.node_id(),
        sql: sql1.to_owned(),
        expected_root: Some(root1),
        signature: node.keypair.sign(&payload1),
    };
    let root2 = node.rpc_write.handle_signed_write(&sw1).await.unwrap();
    assert_ne!(root2, root1);

    // Insert row 2.
    let sql2 = "INSERT INTO counters VALUES (2, 200)";
    let payload2 =
        craftec_sql::rpc_write::build_signed_payload(&node.keypair.node_id(), sql2, Some(root2));
    let sw2 = craftec_sql::SignedWrite {
        writer: node.keypair.node_id(),
        sql: sql2.to_owned(),
        expected_root: Some(root2),
        signature: node.keypair.sign(&payload2),
    };
    let root3 = node.rpc_write.handle_signed_write(&sw2).await.unwrap();
    assert_ne!(root3, root2);

    // Verify final state.
    let rows = node
        .database
        .query("SELECT SUM(val) FROM counters")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        craftec_sql::ColumnValue::Integer(n) => assert_eq!(*n, 300),
        other => panic!("expected Integer(300), got {:?}", other),
    }
}
