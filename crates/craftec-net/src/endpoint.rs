//! [`CraftecEndpoint`] — the main P2P endpoint for the Craftec networking layer.
//!
//! Wraps an `iroh::Endpoint` with Craftec-specific ALPN routing, connection pooling,
//! SWIM membership integration, and message serialization.
//!
//! # Architecture
//!
//! ```text
//!                    ┌─────────────────────────────┐
//!                    │       CraftecEndpoint        │
//!                    │                              │
//!  bootstrap ──────► │  iroh::Endpoint              │
//!  send_message ───► │    └── QUIC uni-streams      │
//!  accept_loop ◄──── │         ALPN routing         │
//!                    │                              │
//!                    │  ConnectionPool (cache)      │
//!                    │  SwimMembership (liveness)   │
//!                    └─────────────────────────────┘
//! ```
//!
//! A single `iroh::Endpoint` is shared for all Craftec protocols.  ALPN tokens
//! distinguish protocol streams:
//!
//! - [`ALPN_CRAFTEC`] — general wire messages (piece exchange, DHT, RPC).
//! - [`ALPN_SWIM`] — SWIM membership traffic.
//!
//! # Usage
//!
//! ```rust,ignore
//! let endpoint = CraftecEndpoint::new(&config, &keypair).await?;
//! endpoint.bootstrap(&config.bootstrap_peers).await?;
//!
//! // Send a message to a known peer
//! endpoint.send_message(&peer_id, &WireMessage::Ping).await?;
//!
//! // Run the accept loop (typically spawned as a background task)
//! endpoint.accept_loop(my_handler).await;
//! ```

use std::sync::Arc;

use craftec_types::hlc::HybridClock;
use craftec_types::{NodeConfig, NodeId, NodeKeypair, WireMessage};

use std::time::Duration;

use crate::connection::{ConnectionHandler, RpcHandler};
use crate::error::{NetError, Result};
use crate::pool::ConnectionPool;
use crate::swim::SwimMembership;

/// Timeout for QUIC stream reads (T11 fix — prevents stalled peers from blocking).
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);

// ── ALPN identifiers ────────────────────────────────────────────────────────

/// ALPN token for general Craftec wire protocol messages.
pub const ALPN_CRAFTEC: &[u8] = b"craftec/0.1";

/// ALPN token for SWIM membership protocol messages.
pub const ALPN_SWIM: &[u8] = b"craftec-swim/0.1";

/// ALPN token for the Craftec RPC protocol (client-facing API).
pub const ALPN_RPC: &[u8] = b"/craftec/rpc/1";

// ── CraftecEndpoint ─────────────────────────────────────────────────────────

/// The main P2P endpoint for Craftec networking.
///
/// Wraps an `iroh::Endpoint` with:
/// - ALPN-based protocol multiplexing.
/// - Transparent connection reuse via [`ConnectionPool`].
/// - Integrated SWIM membership via [`SwimMembership`].
/// - `postcard`-serialised [`WireMessage`] framing over QUIC uni-streams.
///
/// `CraftecEndpoint` is `Clone` — all clones share the same underlying socket,
/// connection pool, and membership table.
#[derive(Clone)]
pub struct CraftecEndpoint {
    endpoint: iroh::Endpoint,
    node_id: NodeId,
    connections: Arc<ConnectionPool>,
    swim: Arc<SwimMembership>,
    /// Hybrid Logical Clock for distributed event ordering (T1).
    hlc: Arc<HybridClock>,
}

impl CraftecEndpoint {
    /// Create a new `CraftecEndpoint`, binding an `iroh::Endpoint` with both ALPN tokens.
    ///
    /// The `keypair` is used as the QUIC/TLS identity.  The iroh `NodeId` is derived
    /// directly from the Ed25519 public key, so `NodeId` == `iroh::PublicKey`.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::ConnectionFailed`] if the iroh endpoint cannot bind.
    pub async fn new(config: &NodeConfig, keypair: &NodeKeypair) -> Result<Self> {
        let secret_key = iroh::SecretKey::from_bytes(&keypair.to_secret_bytes());
        let node_id = keypair.node_id();

        // iroh 0.96: Endpoint::builder().alpns(...).bind() — secret_key sets the Ed25519
        // identity; alpns specifies which ALPN tokens this endpoint accepts for incoming
        // connections.  We bind to the configured listen_port so that peers can reach
        // us at a known address without needing relay fallback.
        let bind_addr = format!("0.0.0.0:{}", config.listen_port);
        let endpoint = iroh::Endpoint::builder()
            .secret_key(secret_key)
            .alpns(vec![
                ALPN_CRAFTEC.to_vec(),
                ALPN_SWIM.to_vec(),
                ALPN_RPC.to_vec(),
            ])
            .clear_ip_transports()
            .bind_addr(bind_addr.as_str())
            .map_err(|e| NetError::ConnectionFailed {
                peer: "self".into(),
                reason: format!("iroh invalid bind addr: {e}"),
            })?
            .bind()
            .await
            .map_err(|e| NetError::ConnectionFailed {
                peer: "self".into(),
                reason: format!("iroh endpoint bind failed: {e}"),
            })?;

        let connections = Arc::new(ConnectionPool::with_capacity(config.max_connections));
        let swim = Arc::new(SwimMembership::new(node_id));
        let hlc = Arc::new(HybridClock::new());

        tracing::info!(
            node_id = %node_id,
            "QUIC endpoint bound — CraftecEndpoint started"
        );

        Ok(Self {
            endpoint,
            node_id,
            connections,
            swim,
            hlc,
        })
    }

    // ── Identity ─────────────────────────────────────────────────────────

    /// Return the iroh `EndpointId` (the Ed25519 public key as an iroh type).
    ///
    /// Equivalent to the local `NodeId` expressed as an iroh type.
    pub fn endpoint_id(&self) -> iroh::PublicKey {
        iroh::PublicKey::from_bytes(self.node_id.as_bytes()).expect("valid 32-byte key")
    }

    /// Return this node's Craftec [`NodeId`].
    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Return a reference to the shared connection pool.
    pub fn connection_pool(&self) -> &Arc<ConnectionPool> {
        &self.connections
    }

    /// Return a reference to the SWIM membership table.
    pub fn swim(&self) -> &Arc<SwimMembership> {
        &self.swim
    }

    /// Return a reference to the Hybrid Logical Clock.
    pub fn hlc(&self) -> &Arc<HybridClock> {
        &self.hlc
    }

    // ── Dialling ─────────────────────────────────────────────────────────

    /// Open a `craftec/0.1` QUIC connection to `addr`.
    ///
    /// Connections are **not** automatically cached here — callers should use
    /// [`CraftecEndpoint::send_message`] which handles pooling transparently.
    ///
    /// # Errors
    ///
    /// - [`NetError::ConnectionFailed`] if the QUIC handshake fails.
    pub async fn connect(&self, addr: iroh::EndpointAddr) -> Result<iroh::endpoint::Connection> {
        tracing::debug!(
            peer = %addr.id,
            "CraftecEndpoint: dialling peer"
        );

        let peer_id = addr.id;
        let conn = self
            .endpoint
            .connect(addr, ALPN_CRAFTEC)
            .await
            .map_err(|e| NetError::ConnectionFailed {
                peer: peer_id.to_string(),
                reason: e.to_string(),
            })?;

        tracing::info!(
            peer = %peer_id,
            "CraftecEndpoint: connection established"
        );
        Ok(conn)
    }

    // ── Sending ───────────────────────────────────────────────────────────

    /// Serialize `msg` with `postcard` and deliver it to `peer` over a QUIC uni-stream.
    ///
    /// Reuses a cached connection from the pool when available; opens a new connection
    /// otherwise and caches it.
    ///
    /// # Errors
    ///
    /// - [`NetError::PeerNotFound`] if `peer` has no entry in the membership table
    ///   and no cached connection (i.e., we have never seen this peer).
    /// - [`NetError::ConnectionFailed`] if a new connection cannot be established.
    /// - [`NetError::SerializationError`] if postcard encoding fails.
    pub async fn send_message(&self, peer: &NodeId, msg: &WireMessage) -> Result<()> {
        tracing::trace!(
            dest = %peer,
            msg_type = msg.type_name(),
            "CraftecEndpoint: sending message"
        );

        // Serialize with frame header + HLC timestamp.
        let hlc_ts = self.hlc.now();
        let bytes = craftec_types::wire::encode_framed_with_hlc(msg, hlc_ts)
            .map_err(|e| NetError::SerializationError(format!("encode_framed failed: {e}")))?;

        // Try the connection pool first.
        let conn = if let Some(cached) = self.connections.get(peer) {
            cached
        } else {
            // We need an EndpointAddr to dial.  Connect with just the EndpointId
            // (iroh resolves it via DNS discovery if configured).
            let addr = iroh::EndpointAddr::from(
                iroh::PublicKey::from_bytes(peer.as_bytes()).expect("valid 32-byte key"),
            );
            let fresh = self.connect(addr).await?;
            self.connections.insert(*peer, fresh.clone());
            // Spawn a reader for the outbound connection so we can receive
            // SWIM PingAck responses and other messages sent back on it.
            tokio::spawn(Self::read_outbound_conn(
                fresh.clone(),
                *peer,
                Arc::clone(&self.swim),
                Arc::clone(&self.hlc),
            ));
            fresh
        };

        // Open a uni-directional stream — fire and forget semantics.
        let mut send_stream = conn
            .open_uni()
            .await
            .map_err(|e| NetError::ConnectionFailed {
                peer: peer.to_string(),
                reason: format!("open_uni failed: {e}"),
            })?;

        send_stream
            .write_all(&bytes)
            .await
            .map_err(|e| NetError::ConnectionFailed {
                peer: peer.to_string(),
                reason: format!("write_all failed: {e}"),
            })?;

        send_stream
            .finish()
            .map_err(|e| NetError::ConnectionFailed {
                peer: peer.to_string(),
                reason: format!("stream finish failed: {e}"),
            })?;

        tracing::debug!(
            dest = %peer,
            bytes = bytes.len(),
            msg_type = msg.type_name(),
            "CraftecEndpoint: message sent"
        );
        Ok(())
    }

    // ── Accepting ─────────────────────────────────────────────────────────

    /// Run the incoming-connection accept loop.
    ///
    /// Blocks until the underlying `iroh::Endpoint` is closed.  For each accepted
    /// connection:
    ///
    /// 1. Reads the ALPN to determine protocol.
    /// 2. Routes `craftec/0.1` connections to `handler`.
    /// 3. Routes `craftec-swim/0.1` connections to the integrated SWIM engine.
    ///
    /// Pass `handler` as an `Arc<impl ConnectionHandler>` for zero-cost clone sharing:
    ///
    /// ```rust,ignore
    /// let ep = endpoint.clone();
    /// let handler = Arc::new(my_handler);
    /// tokio::spawn(async move { ep.accept_loop(handler).await });
    /// ```
    pub async fn accept_loop<H>(&self, handler: Arc<H>, rpc_handler: Option<Arc<dyn RpcHandler>>)
    where
        H: ConnectionHandler,
    {
        tracing::info!(
            node_id = %self.node_id,
            "CraftecEndpoint: accept loop started"
        );

        loop {
            let Some(incoming) = self.endpoint.accept().await else {
                tracing::info!("CraftecEndpoint: accept loop — endpoint closed, exiting");
                break;
            };

            // Accept the connection and inspect the ALPN.
            // In iroh 0.96, accept() yields Option<Accepting>; .await on the
            // Accepting gives a fully-authenticated Connection.
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "CraftecEndpoint: incoming connection failed");
                    continue;
                }
            };

            // Connection::alpn() and Connection::remote_id() are infallible in 0.95+.
            let alpn = conn.alpn();
            let remote_id = conn.remote_id();

            tracing::debug!(
                remote = %remote_id,
                alpn = ?String::from_utf8_lossy(alpn),
                "CraftecEndpoint: accepted connection"
            );

            if alpn == ALPN_CRAFTEC {
                // Cache the connection for outbound reuse.
                let node_id = NodeId::from_bytes(*remote_id.as_bytes());
                self.connections.insert(node_id, conn.clone());

                // Spawn a task to read wire messages from this connection
                // and dispatch them through the application handler.
                let h = Arc::clone(&handler);
                let hlc = Arc::clone(&self.hlc);
                let swim = Arc::clone(&self.swim);
                tokio::spawn(Self::handle_craftec_conn(conn, node_id, h, swim, hlc));
            } else if alpn == ALPN_SWIM {
                let swim = self.swim.clone();
                let node_id = NodeId::from_bytes(*remote_id.as_bytes());
                let hlc = Arc::clone(&self.hlc);
                tokio::spawn(Self::handle_swim_conn(conn, node_id, swim, hlc));
            } else if alpn == ALPN_RPC {
                if let Some(ref rpc) = rpc_handler {
                    let node_id = NodeId::from_bytes(*remote_id.as_bytes());
                    let hlc = Arc::clone(&self.hlc);
                    tokio::spawn(Self::handle_rpc_conn(conn, node_id, rpc.clone(), hlc));
                } else {
                    tracing::warn!(
                        remote = %remote_id,
                        "CraftecEndpoint: RPC connection received but no handler registered"
                    );
                }
            } else {
                tracing::warn!(
                    remote = %remote_id,
                    alpn = ?String::from_utf8_lossy(alpn),
                    "CraftecEndpoint: unknown ALPN — dropping connection"
                );
            }
        }
    }

    // ── Bootstrap ─────────────────────────────────────────────────────────

    /// Connect to bootstrap peers and announce ourselves via `SwimJoin`.
    ///
    /// `peers` is a list of `iroh::NodeAddr` strings in the format produced by
    /// `NodeAddr::to_string()`.  At least one reachable bootstrap peer is required
    /// for the node to join the network.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::BootstrapFailed`] if no bootstrap peer could be reached.
    pub async fn bootstrap(&self, peers: &[String]) -> Result<()> {
        tracing::info!(
            peer_count = peers.len(),
            "CraftecEndpoint: starting bootstrap"
        );

        if peers.is_empty() {
            tracing::warn!("CraftecEndpoint: no bootstrap peers configured — running standalone");
            return Ok(());
        }

        let join_msg = WireMessage::SwimJoin {
            node_id: self.node_id,
            listen_port: 0, // Will be resolved by iroh
        };

        let mut connected = 0usize;

        for peer_str in peers {
            // Parse peer address. Supported formats:
            //   1. <hex_node_id>@<ip>:<port>  — full address with direct IP hint
            //   2. <iroh_public_key>           — bare public key (DNS discovery)
            let (iroh_pk, direct_addr) = if let Some((id_hex, addr_str)) = peer_str.split_once('@')
            {
                // Format: hex_node_id@ip:port
                let bytes = match hex::decode(id_hex) {
                    Ok(b) if b.len() == 32 => {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&b);
                        arr
                    }
                    _ => {
                        tracing::warn!(
                            addr = peer_str,
                            "CraftecEndpoint: bootstrap — invalid hex node ID"
                        );
                        continue;
                    }
                };
                let pk = match iroh::PublicKey::from_bytes(&bytes) {
                    Ok(pk) => pk,
                    Err(e) => {
                        tracing::warn!(
                            addr = peer_str,
                            error = %e,
                            "CraftecEndpoint: bootstrap — invalid public key"
                        );
                        continue;
                    }
                };
                let sock_addr: Option<std::net::SocketAddr> = addr_str.parse().ok();
                (pk, sock_addr)
            } else if let Ok(pk) = peer_str.parse::<iroh::PublicKey>() {
                // Bare public key — rely on DNS discovery.
                (pk, None)
            } else {
                tracing::warn!(
                    addr = peer_str,
                    "CraftecEndpoint: bootstrap — unrecognised peer format (expected <hex_id>@<ip>:<port> or <public_key>)"
                );
                continue;
            };

            let peer_node_id = NodeId::from_bytes(*iroh_pk.as_bytes());
            let addr = if let Some(sock) = direct_addr {
                iroh::EndpointAddr::from(iroh_pk).with_ip_addr(sock)
            } else {
                iroh::EndpointAddr::from(iroh_pk)
            };

            match self.connect(addr).await {
                Ok(conn) => {
                    self.connections.insert(peer_node_id, conn.clone());
                    // Spawn a reader for the bootstrap connection to handle
                    // SWIM responses arriving on the outbound connection.
                    tokio::spawn(Self::read_outbound_conn(
                        conn.clone(),
                        peer_node_id,
                        Arc::clone(&self.swim),
                        Arc::clone(&self.hlc),
                    ));
                    match self.send_message(&peer_node_id, &join_msg).await {
                        Ok(()) => {
                            tracing::info!(
                                peer = %peer_node_id,
                                "CraftecEndpoint: bootstrap — joined via peer"
                            );
                            connected += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer = peer_str,
                                error = %e,
                                "CraftecEndpoint: bootstrap — send_message failed"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        peer = peer_str,
                        error = %e,
                        "CraftecEndpoint: bootstrap — connect failed"
                    );
                }
            }
        }

        if connected == 0 {
            return Err(NetError::BootstrapFailed(format!(
                "could not reach any of {} configured bootstrap peers",
                peers.len()
            )));
        }

        tracing::info!(
            connected,
            total = peers.len(),
            "CraftecEndpoint: bootstrap complete"
        );
        Ok(())
    }

    // ── Private helpers ────────────────────────────────────────────────────

    /// Read wire messages from an outbound connection (handles SWIM acks and
    /// other responses arriving on outbound connections that have no full handler).
    async fn read_outbound_conn(
        conn: iroh::endpoint::Connection,
        remote: NodeId,
        swim: Arc<SwimMembership>,
        hlc: Arc<HybridClock>,
    ) {
        loop {
            let mut stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: outbound conn reader closed");
                    break;
                }
            };
            match tokio::time::timeout(STREAM_READ_TIMEOUT, stream.read_to_end(4 * 1024 * 1024))
                .await
            {
                Ok(Ok(bytes)) => {
                    if let Ok((msg, hlc_ts)) = craftec_types::wire::decode_framed_with_hlc(&bytes) {
                        if hlc_ts > 0 {
                            let _ = hlc.observe(hlc_ts);
                        }
                        if msg.is_swim() {
                            match handle_swim_msg(&msg, &swim) {
                                SwimReply::Send(reply) => {
                                    if !send_swim_reply(&conn, reply, &remote).await {
                                        break;
                                    }
                                }
                                SwimReply::None => {}
                            }
                        } else {
                            tracing::debug!(
                                peer = %remote,
                                msg_type = msg.type_name(),
                                "CraftecEndpoint: outbound conn received non-SWIM message (ignored)"
                            );
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: outbound stream read error");
                    break;
                }
                Err(_) => {
                    tracing::debug!(peer = %remote, "CraftecEndpoint: outbound stream read timed out");
                    continue;
                }
            }
        }
    }

    /// Read wire messages from a `craftec/0.1` connection and dispatch them to `handler`.
    ///
    /// SWIM messages (SwimJoin, SwimPing, SwimPingAck, SwimAlive, SwimSuspect, SwimDead)
    /// arriving on this ALPN are routed to the SWIM membership handler, since
    /// `send_message()` uses the CRAFTEC ALPN for all outbound wire messages.
    async fn handle_craftec_conn<H: ConnectionHandler>(
        conn: iroh::endpoint::Connection,
        remote: NodeId,
        handler: Arc<H>,
        swim: Arc<SwimMembership>,
        hlc: Arc<HybridClock>,
    ) {
        loop {
            let mut stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: uni stream closed");
                    break;
                }
            };

            // Read the full stream payload (bounded to 4 MiB, 30s timeout — T11 fix).
            match tokio::time::timeout(STREAM_READ_TIMEOUT, stream.read_to_end(4 * 1024 * 1024))
                .await
            {
                Ok(Ok(bytes)) => {
                    match craftec_types::wire::decode_framed_with_hlc(&bytes) {
                        Ok((msg, hlc_ts)) => {
                            // Observe remote HLC timestamp (T1).
                            if hlc_ts > 0
                                && let Err(e) = hlc.observe(hlc_ts)
                            {
                                tracing::warn!(
                                    peer = %remote,
                                    error = %e,
                                    "CraftecEndpoint: HLC check failed — dropping message"
                                );
                                continue;
                            }
                            tracing::trace!(
                                peer = %remote,
                                msg_type = msg.type_name(),
                                bytes = bytes.len(),
                                "CraftecEndpoint: received message"
                            );

                            // Route SWIM messages to the membership handler.
                            if msg.is_swim() {
                                match handle_swim_msg(&msg, &swim) {
                                    SwimReply::Send(reply) => {
                                        if !send_swim_reply(&conn, reply, &remote).await {
                                            break;
                                        }
                                    }
                                    SwimReply::None => {}
                                }
                                continue;
                            }

                            // Dispatch non-SWIM messages to the application-layer handler.
                            if let Some(reply) = handler.handle_message(remote, msg).await {
                                match craftec_types::wire::encode_framed(&reply) {
                                    Ok(reply_bytes) => match conn.open_uni().await {
                                        Ok(mut send_stream) => {
                                            if let Err(e) =
                                                send_stream.write_all(&reply_bytes).await
                                            {
                                                tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: reply write failed");
                                            }
                                            let _ = send_stream.finish();
                                        }
                                        Err(e) => {
                                            tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: reply stream open failed");
                                        }
                                    },
                                    Err(e) => {
                                        tracing::warn!(peer = %remote, error = %e, "CraftecEndpoint: reply serialization failed");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer = %remote,
                                error = %e,
                                "CraftecEndpoint: failed to deserialize WireMessage"
                            );
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(peer = %remote, error = %e, "CraftecEndpoint: stream read error");
                    break;
                }
                Err(_) => {
                    tracing::warn!(peer = %remote, "CraftecEndpoint: stream read timed out (30s)");
                    continue;
                }
            }
        }
    }

    /// Maximum concurrent RPC streams per connection (prevents resource exhaustion).
    const MAX_CONCURRENT_RPC_STREAMS: usize = 16;

    /// Handle an incoming RPC connection using bidi streams for request-response.
    async fn handle_rpc_conn(
        conn: iroh::endpoint::Connection,
        remote: NodeId,
        handler: Arc<dyn RpcHandler>,
        hlc: Arc<HybridClock>,
    ) {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(Self::MAX_CONCURRENT_RPC_STREAMS));
        loop {
            let (send, mut recv) = match conn.accept_bi().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: RPC bidi stream closed");
                    break;
                }
            };
            let permit = match semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(peer = %remote, "CraftecEndpoint: RPC stream limit reached ({} concurrent)", Self::MAX_CONCURRENT_RPC_STREAMS);
                    continue;
                }
            };
            let handler = handler.clone();
            let hlc = hlc.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result: std::result::Result<(), anyhow::Error> = async {
                    let bytes = tokio::time::timeout(
                        STREAM_READ_TIMEOUT,
                        recv.read_to_end(4 * 1024 * 1024),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("RPC stream read timed out"))??;

                    let (req, hlc_ts) = craftec_types::wire::decode_rpc_request(&bytes)?;
                    if hlc_ts > 0 {
                        let _ = hlc.observe(hlc_ts);
                    }

                    let resp = handler.handle_request(remote, req).await;
                    let resp_bytes = craftec_types::wire::encode_rpc_response(&resp, hlc.now())?;

                    let mut send = send;
                    send.write_all(&resp_bytes).await?;
                    send.finish()?;
                    Ok(())
                }
                .await;
                if let Err(e) = result {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: RPC stream error");
                }
            });
        }
    }

    /// Read SWIM messages from a `craftec-swim/0.1` connection.
    async fn handle_swim_conn(
        conn: iroh::endpoint::Connection,
        remote: NodeId,
        swim: Arc<SwimMembership>,
        hlc: Arc<HybridClock>,
    ) {
        loop {
            let mut stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: SWIM stream closed");
                    break;
                }
            };

            match tokio::time::timeout(STREAM_READ_TIMEOUT, stream.read_to_end(64 * 1024)).await {
                Ok(Ok(bytes)) => match craftec_types::wire::decode_framed_with_hlc(&bytes) {
                    Ok((msg, hlc_ts)) => {
                        // Observe remote HLC timestamp (T1).
                        if hlc_ts > 0
                            && let Err(e) = hlc.observe(hlc_ts)
                        {
                            tracing::warn!(
                                peer = %remote,
                                error = %e,
                                "CraftecEndpoint: SWIM HLC check failed — dropping"
                            );
                            continue;
                        }
                        // Handle SWIM message without amplification.
                        match handle_swim_msg(&msg, &swim) {
                            SwimReply::Send(reply) => {
                                if !send_swim_reply(&conn, reply, &remote).await {
                                    break;
                                }
                            }
                            SwimReply::None => {}
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            peer = %remote,
                            error = %e,
                            "CraftecEndpoint: SWIM deserialization error"
                        );
                    }
                },
                Ok(Err(e)) => {
                    tracing::warn!(peer = %remote, error = %e, "CraftecEndpoint: SWIM stream read error");
                    break;
                }
                Err(_) => {
                    tracing::warn!(peer = %remote, "CraftecEndpoint: SWIM stream read timed out (30s)");
                    continue;
                }
            }
        }
    }
}

// ── Shared SWIM handler (no amplification) ────────────────────────────────

/// Possible reply to send after handling a SWIM message.
enum SwimReply {
    /// No reply needed (state-only messages).
    None,
    /// Send this message back to the peer.
    Send(WireMessage),
}

/// Handle a single SWIM message without amplification.
///
/// - `SwimPingAck` resolves pending probes and marks alive.
/// - `SwimPing` marks alive, applies piggybacked state, returns a `PingAck`.
/// - `SwimAlive/SwimSuspect/SwimDead` apply state only — no response.
/// - `SwimJoin` marks alive and returns a `SwimAlive` so the joiner learns about us.
///
/// Returns `SwimReply::Send(msg)` if a reply should be sent, `SwimReply::None` otherwise.
fn handle_swim_msg(msg: &WireMessage, swim: &SwimMembership) -> SwimReply {
    match msg {
        WireMessage::SwimPingAck {
            from,
            nonce,
            incarnation,
        } => {
            swim.resolve_probe(*nonce, *incarnation);
            swim.mark_alive(from, *incarnation);
            SwimReply::None
        }
        WireMessage::SwimPing {
            from,
            nonce,
            piggyback,
        } => {
            swim.mark_alive(from, 0);
            for piggybacked_msg in piggyback {
                let _ = swim.handle_message(piggybacked_msg);
            }
            SwimReply::Send(WireMessage::SwimPingAck {
                from: *swim.node_id(),
                nonce: *nonce,
                incarnation: swim.current_incarnation(),
            })
        }
        WireMessage::SwimAlive {
            node_id,
            incarnation,
        } => {
            swim.mark_alive(node_id, *incarnation);
            SwimReply::None
        }
        WireMessage::SwimSuspect {
            node_id,
            incarnation,
            ..
        } => {
            swim.mark_suspect(node_id, *incarnation);
            SwimReply::None
        }
        WireMessage::SwimDead {
            node_id,
            incarnation,
            ..
        } => {
            swim.mark_dead(node_id, *incarnation);
            SwimReply::None
        }
        WireMessage::SwimJoin { node_id, .. } => {
            swim.mark_alive(node_id, 0);
            SwimReply::Send(WireMessage::SwimAlive {
                node_id: *swim.node_id(),
                incarnation: swim.current_incarnation(),
            })
        }
        _ => SwimReply::None,
    }
}

/// Send a SWIM reply over a uni-stream on the given connection.
///
/// Returns `false` if the connection is broken and the caller should break the loop.
async fn send_swim_reply(
    conn: &iroh::endpoint::Connection,
    reply: WireMessage,
    remote: &NodeId,
) -> bool {
    match craftec_types::wire::encode_framed(&reply) {
        Ok(bytes) => match conn.open_uni().await {
            Ok(mut send_stream) => {
                let _ = send_stream.write_all(&bytes).await;
                let _ = send_stream.finish();
                true
            }
            Err(e) => {
                tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: SWIM reply send failed");
                false
            }
        },
        Err(_) => true,
    }
}

// ── RPC client helpers ──────────────────────────────────────────────────────

/// Create an ephemeral iroh endpoint suitable for outbound-only RPC connections.
///
/// Uses a random secret key and no ALPNs (outbound connections specify ALPN at connect time).
pub async fn create_rpc_client_endpoint() -> std::result::Result<iroh::Endpoint, anyhow::Error> {
    // Generate key via OsRng + from_bytes to avoid rand version conflicts
    // (iroh uses rand 0.9, workspace uses rand 0.8).
    let mut key_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key_bytes);
    let secret_key = iroh::SecretKey::from_bytes(&key_bytes);
    let endpoint = iroh::Endpoint::builder()
        .secret_key(secret_key)
        .bind()
        .await?;
    Ok(endpoint)
}

/// Connect to a remote node's RPC endpoint using the `/craftec/rpc/1` ALPN.
///
/// `target_node_id` is the 32-byte Ed25519 public key of the remote node.
/// `target_addr` is a direct socket address (e.g., `127.0.0.1:4433`).
pub async fn rpc_connect(
    endpoint: &iroh::Endpoint,
    target_node_id: &[u8; 32],
    target_addr: std::net::SocketAddr,
) -> std::result::Result<iroh::endpoint::Connection, anyhow::Error> {
    let pk = iroh::PublicKey::from_bytes(target_node_id)?;
    let addr = iroh::EndpointAddr::from(pk).with_ip_addr(target_addr);
    let conn = endpoint.connect(addr, ALPN_RPC).await?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_constants_are_valid_utf8() {
        assert_eq!(std::str::from_utf8(ALPN_CRAFTEC).unwrap(), "craftec/0.1");
        assert_eq!(std::str::from_utf8(ALPN_SWIM).unwrap(), "craftec-swim/0.1");
        assert_eq!(std::str::from_utf8(ALPN_RPC).unwrap(), "/craftec/rpc/1");
    }

    #[test]
    fn alpn_constants_are_distinct() {
        assert_ne!(ALPN_CRAFTEC, ALPN_SWIM);
        assert_ne!(ALPN_CRAFTEC, ALPN_RPC);
        assert_ne!(ALPN_SWIM, ALPN_RPC);
    }
}
