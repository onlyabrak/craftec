//! [`DhtProviders`] — in-process CID → provider record table.
//!
//! iroh 0.96 does not ship a built-in content-routing DHT (see research notes).
//! `DhtProviders` fills this gap with an application-layer provider registry that
//! can be extended later with gossip-based propagation or a Kademlia overlay once
//! the iroh-dht experiment matures.
//!
//! # Current architecture
//!
//! The table is an in-memory `DashMap<Cid, Vec<ProviderRecord>>`.  Announcements from
//! peers are ingested via [`DhtProviders::announce_provider`] and resolved with
//! [`DhtProviders::get_providers`].  There is no persistence or TTL expiry — callers
//! are responsible for removing stale entries via [`DhtProviders::remove_provider`].
//!
//! # Future work
//!
//! - Gossip-based provider record propagation via `iroh-gossip`.
//! - TTL + re-announcement timers.
//! - Kademlia XOR routing once `iroh-dht-experiment` stabilises.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use rand::seq::SliceRandom;

use craftec_types::{Cid, NodeId, WireMessage};

use crate::endpoint::CraftecEndpoint;

// ── Provider record ────────────────────────────────────────────────────────

/// A provider record with a timestamp for TTL-based expiry (T13 fix).
#[derive(Debug, Clone)]
struct ProviderRecord {
    node_id: NodeId,
    announced_at: Instant,
}

// ── Main type ──────────────────────────────────────────────────────────────

/// Manages CID → provider mappings for the local Craftec node.
///
/// Thread-safe and clone-friendly (all clones share state via inner `Arc`).
#[derive(Clone, Default)]
pub struct DhtProviders {
    /// Maps each known CID to the list of provider records (with timestamps).
    local_providers: DashMap<Cid, Vec<ProviderRecord>>,
}

impl DhtProviders {
    /// Create an empty provider table.
    pub fn new() -> Self {
        tracing::debug!("DhtProviders: initialised");
        Self::default()
    }

    /// Record that `node_id` holds (or can serve) `cid`.
    ///
    /// If `node_id` is already a provider for `cid`, refreshes the `announced_at` timestamp.
    pub fn announce_provider(&self, cid: &Cid, node_id: &NodeId) {
        tracing::debug!(
            cid = %cid,
            node = %node_id,
            "DHT: announcing provider"
        );
        self.local_providers
            .entry(*cid)
            .and_modify(|records| {
                if let Some(existing) = records.iter_mut().find(|r| r.node_id == *node_id) {
                    // Refresh timestamp on re-announce.
                    existing.announced_at = Instant::now();
                } else {
                    records.push(ProviderRecord {
                        node_id: *node_id,
                        announced_at: Instant::now(),
                    });
                }
            })
            .or_insert_with(|| {
                vec![ProviderRecord {
                    node_id: *node_id,
                    announced_at: Instant::now(),
                }]
            });
    }

    /// Return the list of nodes known to hold `cid`.
    ///
    /// Returns an empty `Vec` if no providers are recorded for `cid`.
    pub fn get_providers(&self, cid: &Cid) -> Vec<NodeId> {
        let providers: Vec<NodeId> = self
            .local_providers
            .get(cid)
            .map(|records| records.iter().map(|r| r.node_id).collect())
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
        if let Some(mut records) = self.local_providers.get_mut(cid) {
            let before = records.len();
            records.retain(|r| r.node_id != *node_id);
            if records.len() < before {
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
        self.local_providers.retain(|_cid, records| {
            let before = records.len();
            records.retain(|r| r.node_id != *node_id);
            if records.len() < before {
                removed_count += 1;
            }
            !records.is_empty() // drop empty entries
        });
        tracing::debug!(
            node = %node_id,
            cids_affected = removed_count,
            "DHT: node removed from all provider records"
        );
    }

    /// Remove provider records older than `ttl` (T13 fix).
    ///
    /// Returns the number of records pruned.
    pub fn prune_stale(&self, ttl: Duration) -> usize {
        let now = Instant::now();
        let mut pruned = 0usize;

        let cids: Vec<Cid> = self.local_providers.iter().map(|e| *e.key()).collect();

        for cid in cids {
            if let Some(mut records) = self.local_providers.get_mut(&cid) {
                let before = records.len();
                records.retain(|r| now.duration_since(r.announced_at) < ttl);
                pruned += before - records.len();
            }
        }

        // Clean up empty entries.
        self.local_providers
            .retain(|_, records| !records.is_empty());

        if pruned > 0 {
            tracing::debug!(pruned, "DHT: pruned stale provider records");
        }
        pruned
    }

    /// Return the total number of distinct CIDs tracked.
    pub fn cid_count(&self) -> usize {
        self.local_providers.len()
    }

    /// Return the total number of (CID, provider) pairs tracked.
    pub fn provider_count(&self) -> usize {
        self.local_providers.iter().map(|e| e.value().len()).sum()
    }
}

/// Broadcast a [`WireMessage::ProviderAnnounce`] for `cid` to `log(N)` random alive peers.
///
/// Used after a CID is written locally to inform the network that this node holds it.
pub async fn announce_cid_to_peers(cid: &Cid, local_id: &NodeId, endpoint: &CraftecEndpoint) {
    let alive_peers = endpoint.swim().alive_members();
    if alive_peers.is_empty() {
        return;
    }

    let count = ((alive_peers.len() as f64).ln().ceil() as usize).max(1);
    // Scope the non-Send ThreadRng so it's dropped before any .await.
    let targets: Vec<NodeId> = {
        let mut rng = rand::thread_rng();
        alive_peers
            .choose_multiple(&mut rng, count.min(alive_peers.len()))
            .copied()
            .collect()
    };

    let msg = WireMessage::ProviderAnnounce {
        cid: *cid,
        node_id: *local_id,
    };

    for target in &targets {
        if let Err(e) = endpoint.send_message(target, &msg).await {
            tracing::debug!(
                cid = %cid,
                peer = %target,
                error = %e,
                "DHT: failed to announce to peer"
            );
        }
    }

    tracing::debug!(
        cid = %cid,
        announced_to = targets.len(),
        "DHT: CID announced to peers"
    );
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

    #[test]
    fn prune_stale_removes_old_providers() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"stale-test");
        let node = NodeId::generate();

        dht.announce_provider(&cid, &node);
        assert_eq!(dht.provider_count(), 1);

        // Prune with zero TTL — everything is stale.
        let pruned = dht.prune_stale(Duration::ZERO);
        assert_eq!(pruned, 1);
        assert_eq!(dht.provider_count(), 0);
        assert_eq!(dht.cid_count(), 0);
    }

    #[test]
    fn prune_stale_keeps_fresh() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"fresh-test");
        let node = NodeId::generate();

        dht.announce_provider(&cid, &node);

        // Prune with a generous TTL — nothing should be removed.
        let pruned = dht.prune_stale(Duration::from_secs(60));
        assert_eq!(pruned, 0);
        assert_eq!(dht.provider_count(), 1);
    }

    #[test]
    fn announce_refreshes_timestamp() {
        let dht = DhtProviders::new();
        let cid = Cid::from_data(b"refresh-test");
        let node = NodeId::generate();

        dht.announce_provider(&cid, &node);
        // Re-announce should refresh, not duplicate.
        dht.announce_provider(&cid, &node);
        assert_eq!(dht.provider_count(), 1, "re-announce should not duplicate");
        assert_eq!(dht.get_providers(&cid).len(), 1);
    }
}
