//! [`DhtProviders`] — in-process CID → provider record table.
//!
//! iroh 0.96 does not ship a built-in content-routing DHT (see research notes).
//! `DhtProviders` fills this gap with an application-layer provider registry that
//! can be extended later with gossip-based propagation or a Kademlia overlay once
//! the iroh-dht experiment matures.
//!
//! # Current architecture
//!
//! The table is an in-memory `DashMap<Cid, HashSet<NodeId>>`.  Announcements from
//! peers are ingested via [`DhtProviders::announce_provider`] and resolved with
//! [`DhtProviders::get_providers`].  There is no persistence or TTL expiry — callers
//! are responsible for removing stale entries via [`DhtProviders::remove_provider`].
//!
//! # Future work
//!
//! - Gossip-based provider record propagation via `iroh-gossip`.
//! - TTL + re-announcement timers.
//! - Kademlia XOR routing once `iroh-dht-experiment` stabilises.

use std::collections::HashSet;

use dashmap::DashMap;

use craftec_types::{Cid, NodeId};

// ── Main type ──────────────────────────────────────────────────────────────

/// Manages CID → provider mappings for the local Craftec node.
///
/// Thread-safe and clone-friendly (all clones share state via inner `Arc`).
#[derive(Clone, Default)]
pub struct DhtProviders {
    /// Maps each known CID to the set of nodes that have announced holding it.
    local_providers: DashMap<Cid, HashSet<NodeId>>,
}

impl DhtProviders {
    /// Create an empty provider table.
    pub fn new() -> Self {
        tracing::debug!("DhtProviders: initialised");
        Self::default()
    }

    /// Record that `node_id` holds (or can serve) `cid`.
    ///
    /// Idempotent — calling this multiple times with the same arguments is a no-op.
    pub fn announce_provider(&self, cid: &Cid, node_id: &NodeId) {
        tracing::debug!(
            cid = %cid,
            node = %node_id,
            "DHT: announcing provider"
        );
        self.local_providers
            .entry(*cid)
            .or_default()
            .insert(*node_id);
    }

    /// Return the list of nodes known to hold `cid`.
    ///
    /// Returns an empty `Vec` if no providers are recorded for `cid`.
    pub fn get_providers(&self, cid: &Cid) -> Vec<NodeId> {
        let providers: Vec<NodeId> = self
            .local_providers
            .get(cid)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();

        tracing::debug!(
            cid = %cid,
            count = providers.len(),
            "DHT: resolved providers"
        );
        providers
    }

    /// Remove `node_id` as a provider for `cid`.
    ///
    /// If `node_id` was the last provider, the CID entry is also removed.
    pub fn remove_provider(&self, cid: &Cid, node_id: &NodeId) {
        if let Some(mut set) = self.local_providers.get_mut(cid) {
            let removed = set.remove(node_id);
            if removed {
                tracing::debug!(cid = %cid, node = %node_id, "DHT: removed provider");
            }
        }
        // Clean up empty entries to prevent unbounded map growth.
        self.local_providers.remove_if(cid, |_, v| v.is_empty());
    }

    /// Remove `node_id` as a provider for **all** CIDs.
    ///
    /// Called when a node is declared dead by the SWIM membership layer.
    pub fn remove_node(&self, node_id: &NodeId) {
        let mut removed_count = 0usize;
        self.local_providers.retain(|_cid, providers| {
            if providers.remove(node_id) {
                removed_count += 1;
            }
            !providers.is_empty() // drop empty entries
        });
        tracing::debug!(
            node = %node_id,
            cids_affected = removed_count,
            "DHT: node removed from all provider records"
        );
    }

    /// Return the total number of distinct CIDs tracked.
    pub fn cid_count(&self) -> usize {
        self.local_providers.len()
    }

    /// Return the total number of (CID, provider) pairs tracked.
    pub fn provider_count(&self) -> usize {
        self.local_providers
            .iter()
            .map(|e| e.value().len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_and_get() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"test-content");
        let node = NodeId::generate();

        dht.announce_provider(&cid, &node);
        let providers = dht.get_providers(&cid);
        assert_eq!(providers.len(), 1);
        assert!(providers.contains(&node));
    }

    #[test]
    fn get_unknown_cid_returns_empty() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"unknown");
        assert!(dht.get_providers(&cid).is_empty());
    }

    #[test]
    fn announce_idempotent() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"dedup");
        let node = NodeId::generate();

        dht.announce_provider(&cid, &node);
        dht.announce_provider(&cid, &node);
        assert_eq!(dht.get_providers(&cid).len(), 1);
    }

    #[test]
    fn remove_provider() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"removable");
        let n1 = NodeId::generate();
        let n2 = NodeId::generate();

        dht.announce_provider(&cid, &n1);
        dht.announce_provider(&cid, &n2);
        assert_eq!(dht.get_providers(&cid).len(), 2);

        dht.remove_provider(&cid, &n1);
        let remaining = dht.get_providers(&cid);
        assert_eq!(remaining.len(), 1);
        assert!(remaining.contains(&n2));
    }

    #[test]
    fn remove_last_provider_cleans_up_cid() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"last-one");
        let node = NodeId::generate();

        dht.announce_provider(&cid, &node);
        assert_eq!(dht.cid_count(), 1);

        dht.remove_provider(&cid, &node);
        assert_eq!(dht.cid_count(), 0);
    }

    #[test]
    fn remove_node_clears_all_cids() {
        let dht = DhtProviders::new();
        let node = NodeId::generate();
        let cids: Vec<Cid> = (0u8..5).map(|i| Cid::from_data(&[i])).collect();

        for cid in &cids {
            dht.announce_provider(cid, &node);
        }
        assert_eq!(dht.provider_count(), 5);

        dht.remove_node(&node);
        assert_eq!(dht.provider_count(), 0);
    }

    #[test]
    fn multiple_providers_for_same_cid() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"popular-content");
        let nodes: Vec<NodeId> = (0..3).map(|_| NodeId::generate()).collect();

        for n in &nodes {
            dht.announce_provider(&cid, n);
        }
        assert_eq!(dht.get_providers(&cid).len(), 3);
    }
}
