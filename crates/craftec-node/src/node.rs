//! [`CraftecNode`] — top-level orchestrator that composes all Craftec subsystems.
//!
//! # Initialisation order (Join Path, Technical Foundation v3.3 §57)
//!
//! ```text
//! Step  1 — Create / verify data directory
//! Step  2 — Write node.lock sentinel (detect dirty shutdown)
//! Step  3 — KeyStore: load or generate Ed25519 keypair
//! Step  4 — CraftOBJ ContentAddressedStore
//! Step  5 — RLNC engine
//! Step  6 — CID-VFS
//! Step  7 — CraftCOM runtime (Wasmtime) + ProgramScheduler
//! Step  8 — Event bus (broadcast + mpsc channels)
//! Step  9 — iroh Endpoint (CraftecEndpoint, QUIC)
//! Step 10 — SWIM membership table
//! Step 11 — HealthScanner + PieceTracker
//! Step 12 — ProgramScheduler (bound to COM runtime)
//! ```
//!
//! # Run loop
//!
//! After initialisation, [`CraftecNode::run`] spawns background tasks:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │              CraftecNode::run()                      │
//! │                                                      │
//! │  ① bootstrap peers (relay + DNS seeds + static IPs)  │
//! │  ② accept_loop     ← inbound QUIC connections        │
//! │  ③ swim_loop       ← membership protocol ticks       │
//! │  ④ health_loop     ← CID redundancy scan cycles      │
//! │  ⑤ event_loop      ← event bus dispatch              │
//! │                                                      │
//! │  await Ctrl+C / SIGTERM                              │
//! │                                                      │
//! │  graceful shutdown:                                  │
//! │    → broadcast ShutdownSignal event                  │
//! │    → send shutdown_tx broadcast                      │
//! │    → remove node.lock                                │
//! └──────────────────────────────────────────────────────┘
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::broadcast;

use craftec_com::DEFAULT_FUEL_LIMIT;
use craftec_com::runtime::ComRuntime;
use craftec_com::scheduler::ProgramScheduler;
use craftec_crypto::sign::KeyStore;
use craftec_health::repair::RepairExecutor;
use craftec_health::scanner::HealthScanner;
use craftec_health::tracker::PieceTracker;
use craftec_net::dht::DhtProviders;
use craftec_net::endpoint::CraftecEndpoint;
use craftec_net::swim::{SwimMembership, run_swim_loop};
use craftec_obj::ContentAddressedStore;
use craftec_rlnc::engine::RlncEngine;
use craftec_sql::CraftDatabase;
use craftec_sql::RpcWriteHandler;
use craftec_types::config::NodeConfig;
use craftec_types::identity::NodeKeypair;
use craftec_vfs::CidVfs;

use crate::event_bus::EventBus;
use crate::handler::NodeMessageHandler;
use crate::pending::PendingFetches;
use crate::piece_store::CodedPieceIndex;
use crate::rpc::NodeRpcHandler;
use crate::shutdown::wait_for_shutdown;

/// LRU cache capacity for the CraftOBJ content-addressed store.
const OBJ_CACHE_CAPACITY: usize = 1024;

/// Broadcast channel capacity for the shutdown signal.
const SHUTDOWN_CAPACITY: usize = 16;

/// Event bus channel capacity.
const EVENT_BUS_CAPACITY: usize = 1024;

/// Filename for the node lock sentinel inside `data_dir`.
const NODE_LOCK_FILENAME: &str = "node.lock";

// ─── CraftecNode ─────────────────────────────────────────────────────────────

/// The fully-assembled Craftec node.
///
/// Constructed via [`CraftecNode::new`], which initialises every subsystem in
/// dependency order.  Call [`CraftecNode::run`] to bootstrap into the network
/// and block until a shutdown signal is received.
#[allow(dead_code)]
pub struct CraftecNode {
    /// Parsed, validated node configuration.
    config: NodeConfig,
    /// Persistent Ed25519 keypair stored on disk.
    keystore: KeyStore,
    /// Content-addressed local object store (CraftOBJ).
    store: Arc<ContentAddressedStore>,
    /// Random-linear network coding engine.
    rlnc: Arc<RlncEngine>,
    /// CID-based virtual file system for SQLite.
    vfs: Arc<CidVfs>,
    /// The node's own CraftSQL database.
    database: Arc<CraftDatabase>,
    /// RPC write handler for processing signed writes from remote nodes.
    rpc_write_handler: Arc<RpcWriteHandler>,
    /// QUIC/iroh P2P endpoint.
    endpoint: Arc<CraftecEndpoint>,
    /// SWIM membership table.
    swim: Arc<SwimMembership>,
    /// DHT provider record table.
    dht: Arc<DhtProviders>,
    /// Rendezvous point for in-flight piece fetches.
    pending_fetches: Arc<PendingFetches>,
    /// Background CID health scanner.
    health_scanner: Arc<HealthScanner>,
    /// Live coded-piece availability tracker.
    piece_tracker: Arc<PieceTracker>,
    /// Wasmtime agent execution runtime.
    com_runtime: Arc<ComRuntime>,
    /// Kernel-level WASM program lifecycle manager.
    scheduler: Arc<ProgramScheduler>,
    /// Maps content CIDs to their RLNC coded-piece CIDs.
    piece_index: Arc<CodedPieceIndex>,
    /// Internal publish-subscribe event bus.
    event_bus: Arc<EventBus>,
    /// Broadcast sender used to trigger graceful shutdown in background tasks.
    shutdown_tx: broadcast::Sender<()>,
}

impl CraftecNode {
    /// Construct a `CraftecNode` by initialising all subsystems in order.
    ///
    /// Steps match the Join Path from Technical Foundation v3.3, Section 57.
    /// Each step is logged at `INFO` level so the startup sequence is visible
    /// in production logs.
    ///
    /// # Errors
    ///
    /// Returns an error if any subsystem fails to initialise (missing
    /// permissions, corrupt key file, unsupported Wasmtime configuration, etc.).
    pub async fn new(config: NodeConfig) -> Result<Self> {
        let init_start = std::time::Instant::now();

        // ── Step 1: Create / verify data directory ───────────────────────────
        tracing::info!("Step 1: creating/verifying data directory...");
        std::fs::create_dir_all(&config.data_dir).with_context(|| {
            format!(
                "failed to create data directory: {}",
                config.data_dir.display()
            )
        })?;
        tracing::info!(
            path = %config.data_dir.display(),
            "Data directory ready"
        );

        // ── Step 2: Write node.lock sentinel ─────────────────────────────────
        tracing::info!("Step 2: writing node.lock sentinel...");
        let lock_path = config.data_dir.join(NODE_LOCK_FILENAME);
        if lock_path.exists() {
            tracing::warn!(
                path = %lock_path.display(),
                "node.lock already exists — previous shutdown may have been unclean"
            );
        }
        std::fs::write(&lock_path, b"locked")
            .with_context(|| format!("failed to write node.lock at {}", lock_path.display()))?;
        tracing::info!(path = %lock_path.display(), "node.lock written");

        // ── Step 3: Initialize KeyStore ───────────────────────────────────────
        tracing::info!("Step 3: initializing KeyStore (Ed25519 keypair)...");
        let keystore = KeyStore::new(&config.data_dir).context("failed to initialise KeyStore")?;
        tracing::info!(
            node_id = %keystore.node_id(),
            "Ed25519 identity ready (KeyStore loaded)"
        );

        // ── Step 4: Initialize CraftOBJ store ────────────────────────────────
        tracing::info!("Step 4: initializing CraftOBJ ContentAddressedStore...");
        let store = Arc::new(
            ContentAddressedStore::new(&config.data_dir.join("obj"), OBJ_CACHE_CAPACITY)
                .context("failed to initialise ContentAddressedStore")?,
        );
        tracing::info!("CraftOBJ store init complete (ContentAddressedStore ready)");

        // ── Step 5: Initialize RLNC engine ───────────────────────────────────
        tracing::info!("Step 5: initializing RLNC engine...");
        let rlnc = Arc::new(RlncEngine::new());
        tracing::info!("RLNC engine ready");

        // ── Step 6: Initialize CID-VFS ───────────────────────────────────────
        tracing::info!("Step 6: initializing CID-VFS...");
        let vfs = Arc::new(
            CidVfs::new(Arc::clone(&store), config.page_size)
                .context("failed to initialise CidVfs")?,
        );
        tracing::info!(page_size = config.page_size, "CID-VFS ready");

        // ── Step 6b: Initialize CraftSQL database ────────────────────────
        tracing::info!("Step 6b: initializing CraftSQL database...");
        let database = Arc::new(
            CraftDatabase::create(
                keystore.node_id(),
                Arc::clone(&vfs),
                &config.data_dir.join("sql"),
            )
            .await
            .context("failed to initialise CraftDatabase")?,
        );
        let rpc_write_handler = Arc::new(RpcWriteHandler::new(Arc::clone(&database)));
        tracing::info!(
            owner = %keystore.node_id(),
            root_cid = %database.root_cid(),
            "CraftSQL database ready"
        );

        // ── Step 7: Initialize CraftCOM runtime ──────────────────────────────
        tracing::info!("Step 7: initializing CraftCOM Wasmtime runtime...");
        let com_runtime = Arc::new(
            ComRuntime::new(DEFAULT_FUEL_LIMIT).context("failed to initialise ComRuntime")?,
        );
        tracing::info!(fuel_limit = DEFAULT_FUEL_LIMIT, "ComRuntime ready");

        // ── Step 8: Initialize Event Bus ─────────────────────────────────────
        tracing::info!("Step 8: initializing event bus...");
        let event_bus = Arc::new(EventBus::new(EVENT_BUS_CAPACITY));

        // Wire event publishers into subsystems created before the bus.
        store.set_event_sender(event_bus.sender());
        database.set_event_sender(event_bus.sender());

        tracing::info!(capacity = EVENT_BUS_CAPACITY, "Event bus ready");

        // ── Step 9: Initialize iroh Endpoint ─────────────────────────────────
        tracing::info!("Step 9: initializing iroh/QUIC endpoint (CraftecEndpoint)...");
        // Build a NodeKeypair from the stored secret bytes for iroh.
        let keypair = {
            let secret = {
                // Re-open the key file to read the raw bytes for iroh.
                let key_path = config.data_dir.join("node.key");
                let bytes = std::fs::read(&key_path).with_context(|| {
                    format!("failed to read node.key at {}", key_path.display())
                })?;
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                arr
            };
            NodeKeypair::from_secret_bytes(&secret)
        };
        let endpoint = Arc::new(
            CraftecEndpoint::new(&config, &keypair)
                .await
                .context("failed to initialise CraftecEndpoint")?,
        );
        tracing::info!(
            node_id = %endpoint.node_id(),
            port = config.listen_port,
            "CraftecEndpoint ready"
        );

        // Write NodeId hex to data_dir/node.id for CLI discovery.
        let node_id_path = config.data_dir.join("node.id");
        std::fs::write(&node_id_path, endpoint.node_id().to_string())
            .with_context(|| format!("failed to write node.id at {}", node_id_path.display()))?;
        tracing::info!(path = %node_id_path.display(), "node.id written for CLI discovery");

        // ── Step 10: Initialize SWIM membership + DHT ─────────────────────────
        tracing::info!("Step 10: initializing SWIM membership and DHT providers...");
        let swim = endpoint.swim().clone();
        swim.set_event_sender(event_bus.sender());
        let dht = Arc::new(DhtProviders::new());
        let pending_fetches = Arc::new(PendingFetches::new());
        tracing::info!(
            local_id = %keystore.node_id(),
            "SWIM membership, DHT providers, and PendingFetches ready"
        );

        // ── Step 11: Initialize HealthScanner + PieceTracker ─────────────────
        tracing::info!("Step 11: initializing HealthScanner and PieceTracker...");
        let piece_tracker = Arc::new(PieceTracker::new());
        let health_interval = Duration::from_secs(config.health_scan_interval_secs);
        let health_scanner = Arc::new(HealthScanner::new(
            Arc::clone(&store),
            Arc::clone(&piece_tracker),
            health_interval,
        ));
        tracing::info!(
            interval_secs = config.health_scan_interval_secs,
            "HealthScanner and PieceTracker ready"
        );

        // ── Step 11b: Initialize CodedPieceIndex ──────────────────────────────
        let piece_index = Arc::new(CodedPieceIndex::new());
        tracing::info!("CodedPieceIndex ready");

        // ── Step 12: Initialize ProgramScheduler ─────────────────────────────
        tracing::info!("Step 12: initializing ProgramScheduler...");
        let scheduler_keystore = Arc::new(
            KeyStore::new(&config.data_dir).context("failed to initialise scheduler KeyStore")?,
        );
        let scheduler = Arc::new(ProgramScheduler::new(
            Arc::clone(&com_runtime),
            Arc::clone(&store),
            Some(Arc::clone(&database)),
            scheduler_keystore,
        ));
        tracing::info!("ProgramScheduler ready");

        // ── Shutdown channel ──────────────────────────────────────────────────
        let (shutdown_tx, _) = broadcast::channel(SHUTDOWN_CAPACITY);

        tracing::info!(
            node_id = %keystore.node_id(),
            listen_port = config.listen_port,
            total_init_ms = init_start.elapsed().as_millis() as u64,
            "All subsystems initialised — node ready to start"
        );

        Ok(Self {
            config,
            keystore,
            store,
            rlnc,
            vfs,
            database,
            rpc_write_handler,
            endpoint,
            swim,
            dht,
            pending_fetches,
            health_scanner,
            piece_tracker,
            piece_index,
            com_runtime,
            scheduler,
            event_bus,
            shutdown_tx,
        })
    }

    /// Run the node: bootstrap, spawn background tasks, block until shutdown.
    ///
    /// Performs the following steps:
    ///
    /// 1. **Bootstrap** — connect to configured bootstrap peers.
    /// 2. **Accept loop** — spawned task accepts inbound QUIC connections.
    /// 3. **SWIM loop** — spawned task runs membership protocol ticks.
    /// 4. **Health scan loop** — spawned task runs periodic redundancy checks.
    /// 5. **Event dispatch loop** — spawned task processes event bus messages.
    /// 6. **Wait** — main task blocks on Ctrl+C or SIGTERM.
    /// 7. **Graceful shutdown** — sends shutdown broadcast, removes node.lock.
    ///
    /// # Errors
    ///
    /// Returns an error if bootstrapping fails fatally.  Background task
    /// failures are logged but do not propagate here.
    pub async fn run(&self) -> Result<()> {
        tracing::info!("Starting Craftec node...");

        // T15: collect all background tasks in a JoinSet for graceful shutdown.
        let mut tasks = tokio::task::JoinSet::new();

        // ── Bootstrap ─────────────────────────────────────────────────────────
        tracing::info!(
            peers = self.config.bootstrap_peers.len(),
            "Bootstrapping: connecting to initial peers..."
        );
        if !self.config.bootstrap_peers.is_empty() {
            if let Err(e) = self.endpoint.bootstrap(&self.config.bootstrap_peers).await {
                tracing::warn!(error = %e, "Bootstrap encountered errors (continuing)");
            } else {
                tracing::info!("Bootstrap complete");
            }
        } else {
            tracing::info!("No bootstrap peers configured — starting in isolated mode");
        }

        // ── Storage bootstrap: announce locally-held CIDs to DHT (C4: rate-limited) ──
        {
            const STORAGE_BOOTSTRAP_BATCH_SIZE: usize = 100;
            let store = Arc::clone(&self.store);
            let endpoint = Arc::clone(&self.endpoint);
            let node_id = *self.endpoint.node_id();
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tasks.spawn(async move {
                match store.list_cids().await {
                    Ok(cids) => {
                        let total = cids.len();
                        tracing::info!(
                            count = total,
                            "Storage bootstrap: announcing locally-held CIDs"
                        );
                        for (batch_idx, chunk) in
                            cids.chunks(STORAGE_BOOTSTRAP_BATCH_SIZE).enumerate()
                        {
                            for cid in chunk {
                                craftec_net::dht::announce_cid_to_peers(cid, &node_id, &endpoint)
                                    .await;
                            }
                            let announced = (batch_idx + 1) * STORAGE_BOOTSTRAP_BATCH_SIZE;
                            tracing::debug!(
                                batch = batch_idx + 1,
                                batch_size = chunk.len(),
                                progress = format!("{}/{}", announced.min(total), total),
                                "Storage bootstrap: batch announced"
                            );
                            // Rate limit: sleep between batches and check for shutdown.
                            if announced < total {
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                                    _ = shutdown_rx.recv() => {
                                        tracing::info!(
                                            announced = announced.min(total),
                                            total,
                                            "Storage bootstrap: interrupted by shutdown"
                                        );
                                        return;
                                    }
                                }
                            }
                        }
                        tracing::info!(
                            count = total,
                            "Storage bootstrap: CID announcements complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Storage bootstrap: failed to list CIDs"
                        );
                    }
                }
            });
        }
        tracing::info!("Storage bootstrap spawned");

        // ── Spawn: accept loop ────────────────────────────────────────────────
        {
            let endpoint = Arc::clone(&self.endpoint);
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            let handler = Arc::new(NodeMessageHandler::new(
                Arc::clone(&self.store),
                Arc::clone(&self.piece_tracker),
                Arc::clone(&self.dht),
                Arc::clone(&self.pending_fetches),
                Arc::clone(&self.rpc_write_handler),
                Arc::clone(&self.piece_index),
                *self.endpoint.node_id(),
            ));
            let rpc_handler: Arc<dyn craftec_net::RpcHandler> = Arc::new(NodeRpcHandler::new(
                *self.endpoint.node_id(),
                Arc::clone(&self.store),
                Arc::clone(&self.database),
                Arc::clone(&self.swim),
                Arc::clone(&self.piece_tracker),
            ));
            tasks.spawn(async move {
                tracing::info!("Accept loop: starting...");
                tokio::select! {
                    _ = endpoint.accept_loop(handler, Some(rpc_handler)) => {
                        tracing::info!("Accept loop: endpoint closed");
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Accept loop: received shutdown signal");
                    }
                }
            });
        }
        tracing::info!("Accept loop spawned");

        // ── Spawn: SWIM membership loop ───────────────────────────────────────
        {
            let swim = Arc::clone(&self.swim);
            let endpoint = Arc::clone(&self.endpoint);
            let shutdown_rx = self.shutdown_tx.subscribe();
            tasks.spawn(async move {
                tracing::info!("SWIM loop: starting...");
                run_swim_loop(swim, endpoint, shutdown_rx).await;
                tracing::info!("SWIM loop: stopped");
            });
        }
        tracing::info!("SWIM loop spawned");

        // ── Spawn: health scan + repair executor ─────────────────────────────
        {
            let (repair_tx, mut repair_rx) = tokio::sync::mpsc::channel(128);

            // Scanner task: emits RepairRequests into repair_tx.
            let health_scanner = Arc::clone(&self.health_scanner);
            let shutdown_rx = self.shutdown_tx.subscribe();
            tasks.spawn(async move {
                tracing::info!("Health scan loop: starting...");
                health_scanner.run(repair_tx, shutdown_rx).await;
                tracing::info!("Health scan loop: stopped");
            });

            // Repair executor task: consumes RepairRequests from repair_rx.
            let repair_executor = RepairExecutor::new(
                Arc::clone(&self.rlnc),
                Arc::clone(&self.endpoint),
                Arc::clone(&self.piece_tracker),
                Arc::clone(&self.pending_fetches),
            );
            let mut repair_shutdown = self.shutdown_tx.subscribe();
            tasks.spawn(async move {
                tracing::info!("Repair executor: starting...");
                loop {
                    tokio::select! {
                        Some(request) = repair_rx.recv() => {
                            if let Err(e) = repair_executor.execute_repair(&request).await {
                                tracing::warn!(
                                    cid = %request.cid(),
                                    error = %e,
                                    "Repair executor: repair failed"
                                );
                            }
                        }
                        _ = repair_shutdown.recv() => {
                            tracing::info!("Repair executor: shutdown signal received");
                            break;
                        }
                    }
                }
                tracing::info!("Repair executor: stopped");
            });
        }
        tracing::info!("Health scan + repair executor spawned");

        // ── Spawn: event bus dispatch loop ────────────────────────────────────
        {
            let event_bus = Arc::clone(&self.event_bus);
            let mut event_rx = event_bus.subscribe();
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            let endpoint = Arc::clone(&self.endpoint);
            let piece_tracker = Arc::clone(&self.piece_tracker);
            let dht = Arc::clone(&self.dht);
            let store = Arc::clone(&self.store);
            let rlnc = Arc::clone(&self.rlnc);
            let piece_index = Arc::clone(&self.piece_index);
            let node_id = *self.endpoint.node_id();
            tasks.spawn(async move {
                tracing::info!("Event dispatch loop: starting...");
                loop {
                    tokio::select! {
                        result = event_rx.recv() => {
                            match result {
                                Ok(event) => {
                                    tracing::debug!(event = ?event, "Event bus dispatch");
                                    match event {
                                        craftec_types::Event::CidWritten { cid } => {
                                            // DHT announce.
                                            craftec_net::dht::announce_cid_to_peers(
                                                &cid, &node_id, &endpoint,
                                            ).await;

                                            // Skip RLNC encoding for piece CIDs (prevent recursion).
                                            let rlnc_triggered = !piece_index.is_piece_cid(&cid);
                                            tracing::debug!(
                                                cid = %cid,
                                                rlnc_triggered,
                                                "Event: CidWritten dispatched"
                                            );
                                            if !rlnc_triggered {
                                                continue;
                                            }

                                            // C1: RLNC encode off event loop — fire-and-forget spawn.
                                            {
                                                let store = Arc::clone(&store);
                                                let rlnc = Arc::clone(&rlnc);
                                                let piece_index = Arc::clone(&piece_index);
                                                let piece_tracker = Arc::clone(&piece_tracker);
                                                tokio::spawn(async move {
                                                    if let Ok(Some(data)) = store.get(&cid).await {
                                                        let k = 32u32;
                                                        match rlnc.encode(&data, k).await {
                                                            Ok(pieces) => {
                                                                piece_tracker.record_k(&cid, k);
                                                                let mut pcids = Vec::new();
                                                                for piece in &pieces {
                                                                    let bytes = match postcard::to_allocvec(piece) {
                                                                        Ok(b) => b,
                                                                        Err(e) => {
                                                                            tracing::warn!(cid = %cid, error = %e, "RLNC: failed to serialize coded piece — skipping");
                                                                            continue;
                                                                        }
                                                                    };
                                                                    // Pre-compute CID and mark as piece BEFORE store.put fires CidWritten,
                                                                    // preventing the event loop from recursively RLNC-encoding coded pieces.
                                                                    let pcid = craftec_types::Cid::from_data(&bytes);
                                                                    piece_index.mark_piece_cid(pcid);
                                                                    if let Ok(pcid) = store.put(&bytes).await {
                                                                        pcids.push(pcid);
                                                                        piece_tracker.record_piece(
                                                                            &cid,
                                                                            craftec_health::tracker::PieceHolder {
                                                                                node_id,
                                                                                piece_index: pcids.len() as u32 - 1,
                                                                                last_seen: std::time::Instant::now(),
                                                                            },
                                                                        );
                                                                    }
                                                                }
                                                                piece_index.insert(cid, pcids.clone());
                                                                tracing::debug!(
                                                                    cid = %cid,
                                                                    pieces = pieces.len(),
                                                                    "RLNC: encoded and stored coded pieces"
                                                                );
                                                            }
                                                            Err(e) => {
                                                                tracing::warn!(
                                                                    cid = %cid,
                                                                    error = %e,
                                                                    "RLNC: encoding failed"
                                                                );
                                                            }
                                                        }
                                                    }
                                                });
                                            }
                                        }
                                        craftec_types::Event::PeerConnected { node_id } => {
                                            tracing::info!(peer = %node_id, "Event: peer connected");
                                        }
                                        craftec_types::Event::PeerDisconnected { node_id } => {
                                            tracing::info!(peer = %node_id, "Event: peer disconnected");
                                            piece_tracker.remove_node(&node_id);
                                            dht.remove_node(&node_id);
                                        }
                                        craftec_types::Event::RepairNeeded { .. } => {
                                            // Repair is handled via the scanner→executor channel.
                                            tracing::debug!("Event: repair needed (handled by scanner)");
                                        }
                                        craftec_types::Event::DiskWatermarkHit { usage_percent } => {
                                            tracing::warn!(
                                                usage = usage_percent,
                                                "Event: disk watermark hit — eviction agent pending"
                                            );
                                        }
                                        craftec_types::Event::PageCommitted { db_id, page_num, root_cid } => {
                                            tracing::debug!(
                                                db_id = %db_id,
                                                page_num,
                                                root_cid = %root_cid,
                                                "Event: page committed"
                                            );
                                        }
                                        craftec_types::Event::ShutdownSignal => {
                                            tracing::info!("Event dispatch loop: ShutdownSignal received");
                                            break;
                                        }
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                    tracing::warn!(
                                        lagged = n,
                                        "Event dispatch loop lagged — {} events dropped",
                                        n
                                    );
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    tracing::info!("Event bus closed");
                                    break;
                                }
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            tracing::info!("Event dispatch loop: received shutdown signal");
                            break;
                        }
                    }
                }
                tracing::info!("Event dispatch loop: stopped");
            });
        }
        tracing::info!("Event dispatch loop spawned");

        // ── Spawn: PendingFetches pruning (C5) ─────────────────────────────────
        {
            let pending_fetches = Arc::clone(&self.pending_fetches);
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tasks.spawn(async move {
                tracing::info!("PendingFetches pruner: starting...");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {
                            let pruned = pending_fetches.prune_stale(Duration::from_secs(120));
                            if pruned > 0 {
                                tracing::debug!(pruned, "PendingFetches pruner: removed stale entries");
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            tracing::info!("PendingFetches pruner: shutdown signal received");
                            break;
                        }
                    }
                }
                tracing::info!("PendingFetches pruner: stopped");
            });
        }
        tracing::info!("PendingFetches pruner spawned");

        // ── Spawn: DHT provider pruning (C6) ───────────────────────────────────
        {
            let dht = Arc::clone(&self.dht);
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tasks.spawn(async move {
                tracing::info!("DHT provider pruner: starting...");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {
                            let pruned = dht.prune_stale(Duration::from_secs(300));
                            if pruned > 0 {
                                tracing::debug!(pruned, "DHT provider pruner: removed stale records");
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            tracing::info!("DHT provider pruner: shutdown signal received");
                            break;
                        }
                    }
                }
                tracing::info!("DHT provider pruner: stopped");
            });
        }
        tracing::info!("DHT provider pruner spawned");

        tracing::info!(
            node_id = %self.keystore.node_id(),
            listen_port = self.config.listen_port,
            "Craftec node is running — press Ctrl+C or send SIGTERM to stop"
        );

        // ── Wait for shutdown signal ───────────────────────────────────────────
        wait_for_shutdown().await;

        // ── Graceful shutdown sequence (T15: JoinSet instead of fixed sleep) ───
        let shutdown_start = std::time::Instant::now();
        tracing::info!("Graceful shutdown initiated...");

        // Publish ShutdownSignal event to inform any event-bus subscribers.
        self.event_bus
            .publish(craftec_types::event::Event::ShutdownSignal);

        // Broadcast shutdown to all background tasks.
        let _ = self.shutdown_tx.send(());

        // Wait for all tasks to complete, with a 5-second timeout.
        let shutdown_result = tokio::time::timeout(Duration::from_secs(5), async {
            while tasks.join_next().await.is_some() {}
        })
        .await;

        if shutdown_result.is_err() {
            tracing::warn!("Graceful shutdown timed out after 5s — aborting remaining tasks");
            tasks.abort_all();
        }

        // Remove node.lock sentinel.
        let lock_path = self.config.data_dir.join(NODE_LOCK_FILENAME);
        if lock_path.exists() {
            if let Err(e) = std::fs::remove_file(&lock_path) {
                tracing::warn!(
                    error = %e,
                    path = %lock_path.display(),
                    "Failed to remove node.lock during shutdown"
                );
            } else {
                tracing::info!(path = %lock_path.display(), "node.lock removed");
            }
        }

        tracing::info!(
            total_shutdown_ms = shutdown_start.elapsed().as_millis() as u64,
            "Graceful shutdown complete"
        );
        Ok(())
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// The node's configuration.
    #[allow(dead_code)]
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    #[allow(dead_code)]
    /// The node's [`KeyStore`] (Ed25519 identity).
    pub fn keystore(&self) -> &KeyStore {
        &self.keystore
    }

    #[allow(dead_code)]
    /// The content-addressed object store.
    pub fn store(&self) -> &Arc<ContentAddressedStore> {
        &self.store
    }

    #[allow(dead_code)]
    /// The RLNC coding engine.
    pub fn rlnc(&self) -> &Arc<RlncEngine> {
        &self.rlnc
    }

    #[allow(dead_code)]
    /// The CID virtual file system.
    pub fn vfs(&self) -> &Arc<CidVfs> {
        &self.vfs
    }

    #[allow(dead_code)]
    /// The node's CraftSQL database.
    pub fn database(&self) -> &Arc<CraftDatabase> {
        &self.database
    }

    #[allow(dead_code)]
    /// The RPC write handler for signed writes.
    pub fn rpc_write_handler(&self) -> &Arc<RpcWriteHandler> {
        &self.rpc_write_handler
    }

    #[allow(dead_code)]
    /// The QUIC network endpoint.
    pub fn endpoint(&self) -> &Arc<CraftecEndpoint> {
        &self.endpoint
    }

    #[allow(dead_code)]
    /// The SWIM membership table.
    pub fn swim(&self) -> &Arc<SwimMembership> {
        &self.swim
    }

    #[allow(dead_code)]
    /// The DHT provider record table.
    pub fn dht(&self) -> &Arc<DhtProviders> {
        &self.dht
    }

    #[allow(dead_code)]
    /// The pending piece fetches rendezvous point.
    pub fn pending_fetches(&self) -> &Arc<PendingFetches> {
        &self.pending_fetches
    }

    #[allow(dead_code)]
    /// The health scanner.
    pub fn health_scanner(&self) -> &Arc<HealthScanner> {
        &self.health_scanner
    }

    #[allow(dead_code)]
    /// The piece availability tracker.
    pub fn piece_tracker(&self) -> &Arc<PieceTracker> {
        &self.piece_tracker
    }

    #[allow(dead_code)]
    /// The Wasmtime agent execution runtime.
    pub fn com_runtime(&self) -> &Arc<ComRuntime> {
        &self.com_runtime
    }

    #[allow(dead_code)]
    /// The WASM program lifecycle scheduler.
    pub fn scheduler(&self) -> &Arc<ProgramScheduler> {
        &self.scheduler
    }

    #[allow(dead_code)]
    /// The internal event bus.
    pub fn event_bus(&self) -> &Arc<EventBus> {
        &self.event_bus
    }
}
