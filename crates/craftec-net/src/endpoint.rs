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

use craftec_types::{NodeConfig, NodeId, NodeKeypair, WireMessage};

use crate::connection::ConnectionHandler;
use crate::error::{NetError, Result};
use crate::pool::ConnectionPool;
use crate::swim::SwimMembership;

// ── ALPN identifiers ────────────────────────────────────────────────────────

/// ALPN token for general Craftec wire protocol messages.
pub const ALPN_CRAFTEC: &[u8] = b"craftec/0.1";

/// ALPN token for SWIM membership protocol messages.
pub const ALPN_SWIM: &[u8] = b"craftec-swim/0.1";

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
        // connections.
        let endpoint = iroh::Endpoint::builder()
            .secret_key(secret_key)
            .alpns(vec![ALPN_CRAFTEC.to_vec(), ALPN_SWIM.to_vec()])
            .bind()
            .await
            .map_err(|e| NetError::ConnectionFailed {
                peer: "self".into(),
                reason: format!("iroh endpoint bind failed: {e}"),
            })?;

        let connections = Arc::new(ConnectionPool::with_capacity(config.max_connections));
        let swim = Arc::new(SwimMembership::new(node_id));

        tracing::info!(
            node_id = %node_id,
            "CraftecEndpoint: started P2P endpoint"
        );

        Ok(Self {
            endpoint,
            node_id,
            connections,
            swim,
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
        tracing::debug!(
            dest = %peer,
            msg_type = msg.type_name(),
            "CraftecEndpoint: sending message"
        );

        // Serialize first — cheap to fail before touching the network.
        let bytes = postcard::to_allocvec(msg).map_err(|e| {
            NetError::SerializationError(format!("postcard encode failed: {e}"))
        })?;

        // Try the connection pool first.
        let conn = if let Some(cached) = self.connections.get(peer) {
            cached
        } else {
            // We need an EndpointAddr to dial.  Connect with just the EndpointId
            // (iroh resolves it via DNS discovery if configured).
            let addr = iroh::EndpointAddr::from(
                iroh::PublicKey::from_bytes(peer.as_bytes()).expect("valid 32-byte key")
            );
            let fresh = self.connect(addr).await?;
            self.connections.insert(*peer, fresh.clone());
            fresh
        };

        // Open a uni-directional stream — fire and forget semantics.
        let mut send_stream = conn.open_uni().await.map_err(|e| {
            NetError::ConnectionFailed {
                peer: peer.to_string(),
                reason: format!("open_uni failed: {e}"),
            }
        })?;

        send_stream
            .write_all(&bytes)
            .await
            .map_err(|e| NetError::ConnectionFailed {
                peer: peer.to_string(),
                reason: format!("write_all failed: {e}"),
            })?;

        send_stream.finish().map_err(|e| NetError::ConnectionFailed {
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
    pub async fn accept_loop<H>(&self, handler: Arc<H>)
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
                tokio::spawn(Self::handle_craftec_conn(conn, node_id, h));
            } else if alpn == ALPN_SWIM {
                let swim = self.swim.clone();
                let node_id = NodeId::from_bytes(*remote_id.as_bytes());
                tokio::spawn(Self::handle_swim_conn(conn, node_id, swim));
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
            match peer_str.parse::<iroh::PublicKey>() {
                Ok(iroh_pk) => {
                    let peer_node_id = NodeId::from_bytes(*iroh_pk.as_bytes());
                    let addr = iroh::EndpointAddr::from(iroh_pk);
                    match self.connect(addr).await {
                        Ok(conn) => {
                            self.connections.insert(peer_node_id, conn);
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
                                        "CraftecEndpoint: bootstrap — peer unreachable"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer = peer_str,
                                error = %e,
                                "CraftecEndpoint: bootstrap — peer unreachable"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        addr = peer_str,
                        error = %e,
                        "CraftecEndpoint: bootstrap — invalid peer address"
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

    /// Read wire messages from a `craftec/0.1` connection and dispatch them to `handler`.
    async fn handle_craftec_conn<H: ConnectionHandler>(
        conn: iroh::endpoint::Connection,
        remote: NodeId,
        handler: Arc<H>,
    ) {
        loop {
            let mut stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: uni stream closed");
                    break;
                }
            };

            // Read the full stream payload (bounded to 4 MiB to prevent abuse).
            match stream.read_to_end(4 * 1024 * 1024).await {
                Ok(bytes) => {
                    match postcard::from_bytes::<WireMessage>(&bytes) {
                        Ok(msg) => {
                            tracing::debug!(
                                peer = %remote,
                                msg_type = msg.type_name(),
                                bytes = bytes.len(),
                                "CraftecEndpoint: received message"
                            );
                            // Dispatch to the application-layer handler.
                            // Replies are dropped here — full wiring sends them
                            // via send_message if the handler returns Some(reply).
                            let _reply = handler.handle_message(remote, msg).await;
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
                Err(e) => {
                    tracing::warn!(peer = %remote, error = %e, "CraftecEndpoint: stream read error");
                    break;
                }
            }
        }
    }

    /// Read SWIM messages from a `craftec-swim/0.1` connection.
    async fn handle_swim_conn(
        conn: iroh::endpoint::Connection,
        remote: NodeId,
        swim: Arc<SwimMembership>,
    ) {
        loop {
            let mut stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %remote, error = %e, "CraftecEndpoint: SWIM stream closed");
                    break;
                }
            };

            match stream.read_to_end(64 * 1024).await {
                Ok(bytes) => {
                    match postcard::from_bytes::<WireMessage>(&bytes) {
                        Ok(msg) => {
                            let _responses = swim.handle_message(&msg);
                            // Responses would be dispatched back via send_message.
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer = %remote,
                                error = %e,
                                "CraftecEndpoint: SWIM deserialization error"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(peer = %remote, error = %e, "CraftecEndpoint: SWIM stream read error");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_constants_are_valid_utf8() {
        assert_eq!(
            std::str::from_utf8(ALPN_CRAFTEC).unwrap(),
            "craftec/0.1"
        );
        assert_eq!(
            std::str::from_utf8(ALPN_SWIM).unwrap(),
            "craftec-swim/0.1"
        );
    }

    #[test]
    fn alpn_constants_are_distinct() {
        assert_ne!(ALPN_CRAFTEC, ALPN_SWIM);
    }
}
