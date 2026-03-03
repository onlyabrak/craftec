//! [`ConnectionPool`] — reuse-first QUIC connection management.
//!
//! Maintaining a fresh `iroh::Connection` per peer is expensive (TLS handshake,
//! relay round-trip, hole-punching).  The pool caches live connections keyed by
//! [`NodeId`] and evicts idle connections after a configurable timeout.
//!
//! All operations are lock-free via `DashMap`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use iroh::endpoint::Connection;

use craftec_types::NodeId;

/// Default idle timeout: connections unused for 5 minutes are pruned.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum connections held in the pool before new inserts evict the oldest.
const DEFAULT_MAX_CONNECTIONS: usize = 256;

// ── Internal record ────────────────────────────────────────────────────────

/// A single pooled QUIC connection with bookkeeping timestamps.
struct PooledConnection {
    connection: Connection,
    established_at: Instant,
    last_used: Instant,
}

// ── Public API ─────────────────────────────────────────────────────────────

/// A concurrent, lock-free pool of live `iroh` QUIC connections.
///
/// The pool is cheap to clone — all clones share the same underlying map via `Arc`.
#[derive(Clone)]
pub struct ConnectionPool {
    connections: Arc<DashMap<NodeId, PooledConnection>>,
    max_connections: usize,
}

impl ConnectionPool {
    /// Create a new pool with the default capacity (`256`) and idle timeout (`5 min`).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_CONNECTIONS)
    }

    /// Create a pool with an explicit maximum number of concurrent connections.
    pub fn with_capacity(max_connections: usize) -> Self {
        tracing::debug!(max = max_connections, "ConnectionPool: created");
        Self {
            connections: Arc::new(DashMap::new()),
            max_connections,
        }
    }

    /// Retrieve a live connection to `node_id`, updating `last_used` timestamp.
    ///
    /// Returns `None` if no connection for this peer is currently in the pool.
    pub fn get(&self, node_id: &NodeId) -> Option<Connection> {
        if let Some(mut entry) = self.connections.get_mut(node_id) {
            entry.last_used = Instant::now();
            tracing::debug!(peer = %node_id, "ConnectionPool: cache hit");
            Some(entry.connection.clone())
        } else {
            tracing::debug!(peer = %node_id, "ConnectionPool: cache miss");
            None
        }
    }

    /// Insert (or replace) a connection for `node_id`.
    ///
    /// If the pool is at capacity, the least-recently-used connection is evicted before
    /// inserting the new one.
    pub fn insert(&self, node_id: NodeId, conn: Connection) {
        if self.connections.len() >= self.max_connections {
            self.evict_lru();
        }
        let now = Instant::now();
        self.connections.insert(
            node_id,
            PooledConnection {
                connection: conn,
                established_at: now,
                last_used: now,
            },
        );
        tracing::debug!(peer = %node_id, pool_size = self.connections.len(), "ConnectionPool: inserted");
    }

    /// Remove the connection for `node_id` from the pool.
    pub fn remove(&self, node_id: &NodeId) {
        if self.connections.remove(node_id).is_some() {
            tracing::debug!(peer = %node_id, "ConnectionPool: removed");
        }
    }

    /// Return the [`NodeId`]s of all peers with live connections.
    pub fn connected_peers(&self) -> Vec<NodeId> {
        self.connections.iter().map(|e| *e.key()).collect()
    }

    /// Remove connections idle longer than `timeout` and return the evicted node IDs.
    ///
    /// Call this from a periodic maintenance task to prevent stale connections from
    /// accumulating.
    pub fn prune_idle(&self, timeout: Duration) -> Vec<NodeId> {
        let cutoff = Instant::now() - timeout;
        let mut evicted = Vec::new();

        self.connections.retain(|node_id, entry| {
            if entry.last_used < cutoff {
                tracing::debug!(
                    peer = %node_id,
                    idle_secs = entry.last_used.elapsed().as_secs(),
                    "ConnectionPool: pruning idle connection"
                );
                evicted.push(*node_id);
                false // drop from map
            } else {
                true // keep
            }
        });

        if !evicted.is_empty() {
            tracing::info!(pruned = evicted.len(), "ConnectionPool: idle prune complete");
        }
        evicted
    }

    /// Number of connections currently in the pool.
    #[inline]
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Returns `true` if the pool holds no connections.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

// ── Private helpers ────────────────────────────────────────────────────────

impl ConnectionPool {
    /// Evict the connection with the oldest `last_used` timestamp (LRU eviction).
    fn evict_lru(&self) {
        let oldest_key = self
            .connections
            .iter()
            .min_by_key(|e| e.last_used)
            .map(|e| *e.key());

        if let Some(key) = oldest_key {
            self.connections.remove(&key);
            tracing::debug!(peer = %key, "ConnectionPool: LRU eviction");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify basic pool operations compile and run — we cannot instantiate real
    /// `iroh::Connection`s in unit tests, so we test the bookkeeping logic only via
    /// the `connected_peers` / `len` / `is_empty` surface.
    #[test]
    fn pool_starts_empty() {
        let pool = ConnectionPool::new();
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
        assert!(pool.connected_peers().is_empty());
    }

    #[test]
    fn get_on_empty_returns_none() {
        let pool = ConnectionPool::new();
        let id = NodeId::generate();
        assert!(pool.get(&id).is_none());
    }

    #[test]
    fn remove_on_missing_is_noop() {
        let pool = ConnectionPool::new();
        let id = NodeId::generate();
        pool.remove(&id); // should not panic
    }

    #[test]
    fn prune_idle_on_empty_returns_empty() {
        let pool = ConnectionPool::new();
        let evicted = pool.prune_idle(DEFAULT_IDLE_TIMEOUT);
        assert!(evicted.is_empty());
    }
}
