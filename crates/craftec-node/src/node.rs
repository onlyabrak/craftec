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

use craftec_com::runtime::ComRuntime;
use craftec_com::scheduler::ProgramScheduler;
use craftec_com::DEFAULT_FUEL_LIMIT;
use craftec_crypto::sign::KeyStore;
use craftec_health::scanner::HealthScanner;
use craftec_health::tracker::PieceTracker;
use craftec_net::connection::HandlerFuture;
use craftec_net::endpoint::CraftecEndpoint;
use craftec_net::swim::{run_swim_loop, SwimMembership};
use craftec_obj::ContentAddressedStore;
use craftec_rlnc::engine::RlncEngine;
use craftec_types::config::NodeConfig;
use craftec_types::identity::NodeKeypair;
use craftec_vfs::CidVfs;

use crate::event_bus::EventBus;
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
    /// QUIC/iroh P2P endpoint.
    endpoint: Arc<CraftecEndpoint>,
    /// SWIM membership table.
    swim: Arc<SwimMembership>,
    /// Background CID health scanner.
    health_scanner: Arc<HealthScanner>,
    /// Live coded-piece availability tracker.
    piece_tracker: Arc<PieceTracker>,
    /// Wasmtime agent execution runtime.
    com_runtime: Arc<ComRuntime>,
    /// Kernel-level WASM program lifecycle manager.
    scheduler: Arc<ProgramScheduler>,
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
        let keystore = KeyStore::new(&config.data_dir)
            .context("failed to initialise KeyStore")?;
        tracing::info!(
            node_id = %keystore.node_id(),
            "KeyStore ready"
        );

        // ── Step 4: Initialize CraftOBJ store ────────────────────────────────
        tracing::info!("Step 4: initializing CraftOBJ ContentAddressedStore...");
        let store = Arc::new(
            ContentAddressedStore::new(&config.data_dir.join("obj"), OBJ_CACHE_CAPACITY)
                .context("failed to initialise ContentAddressedStore")?,
        );
        tracing::info!("ContentAddressedStore ready");

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

        // ── Step 7: Initialize CraftCOM runtime ──────────────────────────────
        tracing::info!("Step 7: initializing CraftCOM Wasmtime runtime...");
        let com_runtime = Arc::new(
            ComRuntime::new(DEFAULT_FUEL_LIMIT)
                .context("failed to initialise ComRuntime")?,
        );
        tracing::info!(fuel_limit = DEFAULT_FUEL_LIMIT, "ComRuntime ready");

        // ── Step 8: Initialize Event Bus ─────────────────────────────────────
        tracing::info!("Step 8: initializing event bus...");
        let event_bus = Arc::new(EventBus::new(EVENT_BUS_CAPACITY));
        tracing::info!(capacity = EVENT_BUS_CAPACITY, "Event bus ready");

        // ── Step 9: Initialize iroh Endpoint ─────────────────────────────────
        tracing::info!("Step 9: initializing iroh/QUIC endpoint (CraftecEndpoint)...");
        // Build a NodeKeypair from the stored secret bytes for iroh.
        let keypair = {
            let secret = {
                // Re-open the key file to read the raw bytes for iroh.
                let key_path = config.data_dir.join("node.key");
                let bytes = std::fs::read(&key_path)
                    .with_context(|| format!("failed to read node.key at {}", key_path.display()))?;
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

        // ── Step 10: Initialize SWIM membership ──────────────────────────────
        tracing::info!("Step 10: initializing SWIM membership...");
        let swim = endpoint.swim().clone();
        tracing::info!(
            local_id = %keystore.node_id(),
            "SWIM membership ready"
        );

        // ── Step 11: Initialize HealthScanner + PieceTracker ─────────────────
        tracing::info!("Step 11: initializing HealthScanner and PieceTracker...");
        let piece_tracker = Arc::new(PieceTracker::new());
        let health_interval =
            Duration::from_secs(config.health_scan_interval_secs);
        let health_scanner = Arc::new(HealthScanner::new(
            Arc::clone(&store),
            Arc::clone(&piece_tracker),
            health_interval,
        ));
        tracing::info!(
            interval_secs = config.health_scan_interval_secs,
            "HealthScanner and PieceTracker ready"
        );

        // ── Step 12: Initialize ProgramScheduler ─────────────────────────────
        tracing::info!("Step 12: initializing ProgramScheduler...");
        let scheduler = Arc::new(ProgramScheduler::new(Arc::clone(&com_runtime)));
        tracing::info!("ProgramScheduler ready");

        // ── Shutdown channel ──────────────────────────────────────────────────
        let (shutdown_tx, _) = broadcast::channel(SHUTDOWN_CAPACITY);

        tracing::info!(
            node_id = %keystore.node_id(),
            listen_port = config.listen_port,
            "All subsystems initialised — node ready to start"
        );

        Ok(Self {
            config,
            keystore,
            store,
            rlnc,
            vfs,
            endpoint,
            swim,
            health_scanner,
            piece_tracker,
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

        // ── Spawn: accept loop ────────────────────────────────────────────────
        {
            let endpoint = Arc::clone(&self.endpoint);
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
                tracing::info!("Accept loop: starting...");
                // A no-op handler that logs and discards all inbound messages.
                // Subsystems register their own handlers in a future refactor.
                struct LoggingHandler;
                impl craftec_net::ConnectionHandler for LoggingHandler {
                    fn handle_message(
                        &self,
                        from: craftec_types::NodeId,
                        msg: craftec_types::WireMessage,
                    ) -> HandlerFuture {
                        tracing::debug!(peer = %from, msg = ?msg, "Accept loop: inbound message");
                        Box::pin(async move { None })
                    }
                }
                let handler = Arc::new(LoggingHandler);
                tokio::select! {
                    _ = endpoint.accept_loop(handler) => {
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
            let shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
                tracing::info!("SWIM loop: starting...");
                run_swim_loop(swim, shutdown_rx).await;
                tracing::info!("SWIM loop: stopped");
            });
        }
        tracing::info!("SWIM loop spawned");

        // ── Spawn: health scan loop ───────────────────────────────────────────
        {
            let health_scanner = Arc::clone(&self.health_scanner);
            let shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
                tracing::info!("Health scan loop: starting...");
                health_scanner.run(shutdown_rx).await;
                tracing::info!("Health scan loop: stopped");
            });
        }
        tracing::info!("Health scan loop spawned");

        // ── Spawn: event bus dispatch loop ────────────────────────────────────
        {
            let event_bus = Arc::clone(&self.event_bus);
            let mut event_rx = event_bus.subscribe();
            let mut shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(async move {
                tracing::info!("Event dispatch loop: starting...");
                loop {
                    tokio::select! {
                        result = event_rx.recv() => {
                            match result {
                                Ok(event) => {
                                    tracing::debug!(event = ?event, "Event bus dispatch");
                                    // Subsystems subscribe directly; this loop
                                    // is for central logging / diagnostics only.
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

        tracing::info!(
            node_id = %self.keystore.node_id(),
            listen_port = self.config.listen_port,
            "Craftec node is running — press Ctrl+C or send SIGTERM to stop"
        );

        // ── Wait for shutdown signal ───────────────────────────────────────────
        wait_for_shutdown().await;

        // ── Graceful shutdown sequence ─────────────────────────────────────────
        tracing::info!("Graceful shutdown initiated...");

        // Publish ShutdownSignal event to inform any event-bus subscribers.
        self.event_bus.publish(craftec_types::event::Event::ShutdownSignal);

        // Broadcast shutdown to all background tasks.
        let _ = self.shutdown_tx.send(());

        // Brief yield to let tasks react to the shutdown signal.
        tokio::time::sleep(Duration::from_millis(500)).await;

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

        tracing::info!("Graceful shutdown complete");
        Ok(())
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// The node's configuration.
    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// The node's [`KeyStore`] (Ed25519 identity).
    pub fn keystore(&self) -> &KeyStore {
        &self.keystore
    }

    /// The content-addressed object store.
    pub fn store(&self) -> &Arc<ContentAddressedStore> {
        &self.store
    }

    /// The RLNC coding engine.
    pub fn rlnc(&self) -> &Arc<RlncEngine> {
        &self.rlnc
    }

    /// The CID virtual file system.
    pub fn vfs(&self) -> &Arc<CidVfs> {
        &self.vfs
    }

    /// The QUIC network endpoint.
    pub fn endpoint(&self) -> &Arc<CraftecEndpoint> {
        &self.endpoint
    }

    /// The SWIM membership table.
    pub fn swim(&self) -> &Arc<SwimMembership> {
        &self.swim
    }

    /// The health scanner.
    pub fn health_scanner(&self) -> &Arc<HealthScanner> {
        &self.health_scanner
    }

    /// The piece availability tracker.
    pub fn piece_tracker(&self) -> &Arc<PieceTracker> {
        &self.piece_tracker
    }

    /// The Wasmtime agent execution runtime.
    pub fn com_runtime(&self) -> &Arc<ComRuntime> {
        &self.com_runtime
    }

    /// The WASM program lifecycle scheduler.
    pub fn scheduler(&self) -> &Arc<ProgramScheduler> {
        &self.scheduler
    }

    /// The internal event bus.
    pub fn event_bus(&self) -> &Arc<EventBus> {
        &self.event_bus
    }
}
