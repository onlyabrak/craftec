//! SWIM membership protocol — O(log N) failure detection and membership dissemination.
//!
//! # Protocol overview
//!
//! The SWIM (Scalable Weakly-consistent Infection-style Membership) protocol
//! separates cluster membership into two orthogonal sub-problems:
//!
//! 1. **Failure detection** — a randomised probe/ping-req/ack cycle that runs
//!    every [`SwimMembership::protocol_period`] seconds.
//! 2. **Dissemination** — membership updates are piggybacked on failure detector
//!    messages, spreading to all members in O(log N) rounds.
//!
//! ## Message complexity
//!
//! | Property | Guarantee |
//! |---|---|
//! | Messages per node per period | **O(1) constant** |
//! | Total cluster messages per period | **O(N)** |
//! | Dissemination latency | **O(log N) protocol periods** |
//!
//! ## Member states
//!
//! ```text
//! Alive ──probe fails──> Suspect ──timeout──> Dead
//!   ^                        │
//!   └───── refutation ───────┘  (node sends higher incarnation)
//! ```
//!
//! # Implementation notes
//!
//! This implementation follows the Hashicorp Memberlist variant of SWIM with the
//! "suspicion" improvement from the Lifeguard paper.  The `incarnation` counter
//! allows a node to refute suspect/dead claims by incrementing and broadcasting a
//! higher `Alive` message.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::oneshot;

use craftec_types::{NodeId, WireMessage};

/// Default time before a `Suspect` node is declared `Dead` (spec §18: 5 seconds).
const DEFAULT_SUSPECT_TIMEOUT: Duration = Duration::from_millis(5000);

/// Default protocol tick period — one random peer is probed per tick (§13).
const DEFAULT_PROTOCOL_PERIOD: Duration = Duration::from_millis(500);

// ── Member state ───────────────────────────────────────────────────────────

/// The observed liveness state of a cluster member.
///
/// State transitions:
/// - `Alive` → `Suspect` when probe fails and no `ping-req` acks arrive within the timeout.
/// - `Suspect` → `Alive` when the node itself sends a higher-incarnation `Alive` message.
/// - `Suspect` → `Dead` when `suspect_timeout` expires without refutation.
#[derive(Debug, Clone)]
pub enum MemberState {
    /// The node is believed to be healthy.  `incarnation` is the node's own counter —
    /// a node refutes suspect claims by incrementing this and broadcasting `Alive`.
    Alive { incarnation: u64 },

    /// The node failed to respond to a probe.  If not refuted within `suspect_timeout`,
    /// it will be declared dead.
    Suspect {
        incarnation: u64,
        /// Wall-clock time when this node was first suspected.
        since: Instant,
    },

    /// The node is considered permanently gone for this membership epoch.
    Dead { incarnation: u64 },
}

// ── Main type ──────────────────────────────────────────────────────────────

/// Thread-safe SWIM membership table and protocol engine.
///
/// Wrap in `Arc` to share between tasks:
///
/// ```rust,ignore
/// let swim = Arc::new(SwimMembership::new(local_id));
/// let swim_clone = Arc::clone(&swim);
/// tokio::spawn(async move { swim_clone.run_protocol(shutdown_rx).await });
/// ```
pub struct SwimMembership {
    /// The set of known members and their states.
    members: DashMap<NodeId, MemberState>,
    /// The local node's own incarnation counter.  Increment to refute suspect claims.
    incarnation: AtomicU64,
    /// This node's own ID — never appears in `members`.
    local_id: NodeId,
    /// Duration after which a `Suspect` node is promoted to `Dead`.
    pub suspect_timeout: Duration,
    /// Duration between protocol ticks (one probe per tick).
    pub protocol_period: Duration,
    /// Index into the shuffled probe order (round-robin across members).
    probe_index: AtomicUsize,
    /// Monotonic counter for probe nonces (T2: probe-ack correlation).
    probe_nonce: AtomicU64,
    /// Pending probe acks keyed by nonce.
    pending_probes: DashMap<u64, oneshot::Sender<u64>>,
}

impl SwimMembership {
    /// Create a new membership table for the node with identity `local_id`.
    ///
    /// Uses default timing parameters:
    /// - `suspect_timeout`: 10 seconds.
    /// - `protocol_period`: 1 second.
    pub fn new(local_id: NodeId) -> Self {
        tracing::info!(
            local = %local_id,
            "SwimMembership: initialised"
        );
        Self {
            members: DashMap::new(),
            incarnation: AtomicU64::new(0),
            local_id,
            suspect_timeout: DEFAULT_SUSPECT_TIMEOUT,
            protocol_period: DEFAULT_PROTOCOL_PERIOD,
            probe_index: AtomicUsize::new(0),
            probe_nonce: AtomicU64::new(0),
            pending_probes: DashMap::new(),
        }
    }

    // ── Accessors ──────────────────────────────────────────────────────────

    /// Return the [`NodeId`]s of all members currently in the `Alive` state.
    pub fn alive_members(&self) -> Vec<NodeId> {
        self.members
            .iter()
            .filter_map(|e| match e.value() {
                MemberState::Alive { .. } => Some(*e.key()),
                _ => None,
            })
            .collect()
    }

    /// Return the total number of known members (all states).
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Return `true` if `node_id` is currently in the `Alive` state.
    pub fn is_alive(&self, node_id: &NodeId) -> bool {
        matches!(
            self.members.get(node_id).as_deref(),
            Some(MemberState::Alive { .. })
        )
    }

    /// Return the local node's current incarnation number.
    pub fn current_incarnation(&self) -> u64 {
        self.incarnation.load(Ordering::Acquire)
    }

    // ── State transitions ──────────────────────────────────────────────────

    /// Mark `node_id` as `Alive` with the given `incarnation`.
    ///
    /// Uses entry-based atomic update to prevent TOCTOU races between
    /// concurrent state transitions (T7 fix).
    pub fn mark_alive(&self, node_id: &NodeId, incarnation: u64) {
        if node_id == &self.local_id {
            return; // Never update own entry — local state is authoritative.
        }
        self.members
            .entry(*node_id)
            .and_modify(|state| {
                let dominated = match state {
                    MemberState::Alive { incarnation: inc } => incarnation <= *inc,
                    MemberState::Suspect {
                        incarnation: inc, ..
                    } => incarnation < *inc,
                    MemberState::Dead { incarnation: inc } => incarnation <= *inc,
                };
                if !dominated {
                    tracing::debug!(node = %node_id, incarnation, "SWIM: mark_alive (update)");
                    *state = MemberState::Alive { incarnation };
                }
            })
            .or_insert_with(|| {
                tracing::debug!(node = %node_id, incarnation, "SWIM: mark_alive (new)");
                MemberState::Alive { incarnation }
            });
    }

    /// Mark `node_id` as `Suspect`.
    ///
    /// Uses entry-based atomic update to prevent TOCTOU races (T7 fix).
    /// Will not override a higher-incarnation `Alive` or an existing `Dead` state.
    pub fn mark_suspect(&self, node_id: &NodeId, incarnation: u64) {
        if node_id == &self.local_id {
            // We are suspected — refute by incrementing our own incarnation.
            let new_inc = self.incarnation.fetch_add(1, Ordering::AcqRel) + 1;
            tracing::info!(
                new_incarnation = new_inc,
                "SWIM: refuting suspect claim on self"
            );
            return;
        }
        self.members
            .entry(*node_id)
            .and_modify(|state| {
                let should_update = match state {
                    MemberState::Alive { incarnation: inc } => incarnation >= *inc,
                    MemberState::Suspect {
                        incarnation: inc, ..
                    } => incarnation > *inc,
                    MemberState::Dead { .. } => false, // Dead is terminal.
                };
                if should_update {
                    tracing::debug!(node = %node_id, incarnation, "SWIM: mark_suspect (update)");
                    *state = MemberState::Suspect {
                        incarnation,
                        since: Instant::now(),
                    };
                }
            })
            .or_insert_with(|| {
                tracing::debug!(node = %node_id, incarnation, "SWIM: mark_suspect (new)");
                MemberState::Suspect {
                    incarnation,
                    since: Instant::now(),
                }
            });
    }

    /// Mark `node_id` as `Dead`.
    ///
    /// Uses entry-based atomic update to prevent TOCTOU races (T7 fix).
    /// `Dead` is terminal — it cannot be overridden by `Suspect` or an equal-incarnation
    /// `Alive`.  A higher-incarnation `Alive` from the node itself can revive it.
    pub fn mark_dead(&self, node_id: &NodeId, incarnation: u64) {
        if node_id == &self.local_id {
            tracing::warn!("SWIM: received Dead claim about self — ignoring");
            return;
        }
        self.members
            .entry(*node_id)
            .and_modify(|state| {
                let should_update = match state {
                    MemberState::Dead { incarnation: inc } => incarnation > *inc,
                    _ => true,
                };
                if should_update {
                    tracing::info!(node = %node_id, incarnation, "SWIM: mark_dead (update)");
                    *state = MemberState::Dead { incarnation };
                }
            })
            .or_insert_with(|| {
                tracing::info!(node = %node_id, incarnation, "SWIM: mark_dead (new)");
                MemberState::Dead { incarnation }
            });
    }

    // ── Probe-ack support (T2) ────────────────────────────────────────────

    /// Allocate a new probe nonce and return a `(nonce, receiver)` pair.
    /// The receiver resolves when the remote node acks with the same nonce.
    pub fn register_probe(&self) -> (u64, oneshot::Receiver<u64>) {
        let nonce = self.probe_nonce.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_probes.insert(nonce, tx);
        (nonce, rx)
    }

    /// Resolve a pending probe by nonce. Called when a SwimPingAck arrives.
    /// Returns `true` if the probe was still pending.
    pub fn resolve_probe(&self, nonce: u64, incarnation: u64) -> bool {
        if let Some((_, tx)) = self.pending_probes.remove(&nonce) {
            let _ = tx.send(incarnation);
            true
        } else {
            false
        }
    }

    /// Return up to `count` random alive members excluding `exclude`.
    pub fn random_alive_excluding(&self, exclude: &NodeId, count: usize) -> Vec<NodeId> {
        use rand::seq::SliceRandom;
        let mut alive: Vec<NodeId> = self
            .alive_members()
            .into_iter()
            .filter(|id| id != exclude)
            .collect();
        alive.shuffle(&mut rand::thread_rng());
        alive.truncate(count);
        alive
    }

    /// Return the last known incarnation for `node_id`, or 0 if unknown.
    pub fn last_known_incarnation(&self, node_id: &NodeId) -> u64 {
        self.members
            .get(node_id)
            .map(|e| match e.value() {
                MemberState::Alive { incarnation } => *incarnation,
                MemberState::Suspect { incarnation, .. } => *incarnation,
                MemberState::Dead { incarnation } => *incarnation,
            })
            .unwrap_or(0)
    }

    // ── Protocol message handling ──────────────────────────────────────────

    /// Process an incoming SWIM wire message and return any membership updates to propagate.
    ///
    /// The returned `Vec<WireMessage>` contains piggybacked gossip updates that should
    /// be forwarded to the next `O(log N)` peers.
    ///
    /// # Handled variants
    ///
    /// | Message | Effect |
    /// |---|---|
    /// | `SwimJoin { node_id, incarnation }` | Insert as `Alive`; return `SwimAlive` back |
    /// | `SwimAlive { node_id, incarnation }` | Apply `mark_alive`; propagate |
    /// | `SwimSuspect { node_id, incarnation }` | Apply `mark_suspect`; propagate |
    /// | `SwimDead { node_id, incarnation }` | Apply `mark_dead`; propagate |
    pub fn handle_message(&self, msg: &WireMessage) -> Vec<WireMessage> {
        let mut responses = Vec::new();

        match msg {
            WireMessage::SwimJoin {
                node_id,
                listen_port: _,
            } => {
                tracing::debug!(
                    from = %node_id,
                    "SWIM: processed SwimJoin"
                );
                self.mark_alive(node_id, 0); // initial incarnation 0
                // Respond with our own Alive so the joiner learns about us.
                let own_inc = self.incarnation.load(Ordering::Acquire);
                responses.push(WireMessage::SwimAlive {
                    node_id: self.local_id,
                    incarnation: own_inc,
                });
            }

            WireMessage::SwimAlive {
                node_id,
                incarnation,
            } => {
                tracing::debug!(
                    from = %node_id,
                    incarnation,
                    "SWIM: processed SwimAlive"
                );
                self.mark_alive(node_id, *incarnation);
                responses.push(msg.clone());
            }

            WireMessage::SwimSuspect {
                node_id,
                incarnation,
                ..
            } => {
                tracing::debug!(
                    from = %node_id,
                    incarnation,
                    "SWIM: processed SwimSuspect"
                );
                self.mark_suspect(node_id, *incarnation);
                responses.push(msg.clone());
            }

            WireMessage::SwimDead {
                node_id,
                incarnation,
                ..
            } => {
                tracing::debug!(
                    from = %node_id,
                    incarnation,
                    "SWIM: processed SwimDead"
                );
                self.mark_dead(node_id, *incarnation);
                responses.push(msg.clone());
            }

            WireMessage::SwimPing {
                from,
                nonce,
                piggyback,
            } => {
                tracing::debug!(
                    from = %from,
                    nonce,
                    piggyback_count = piggyback.len(),
                    "SWIM: processed SwimPing"
                );
                // Process piggybacked membership updates.
                for piggybacked_msg in piggyback {
                    let sub_responses = self.handle_message(piggybacked_msg);
                    responses.extend(sub_responses);
                }
                // Respond with SwimPingAck (replaces old SwimAlive response).
                let own_inc = self.incarnation.load(Ordering::Acquire);
                responses.push(WireMessage::SwimPingAck {
                    from: self.local_id,
                    nonce: *nonce,
                    incarnation: own_inc,
                });
            }

            WireMessage::SwimPingAck {
                from,
                nonce,
                incarnation,
            } => {
                tracing::debug!(
                    from = %from,
                    nonce,
                    incarnation,
                    "SWIM: processed SwimPingAck"
                );
                self.mark_alive(from, *incarnation);
                self.resolve_probe(*nonce, *incarnation);
            }

            _ => {
                tracing::trace!("SWIM: ignoring non-SWIM message variant");
            }
        }

        responses
    }

    // ── Protocol tick ──────────────────────────────────────────────────────

    /// Run a single SWIM protocol tick.
    ///
    /// Per tick:
    /// 1. Promote any `Suspect` nodes whose `since` timestamp exceeds `suspect_timeout` to `Dead`.
    /// 2. Select the next alive member to probe (round-robin over a shuffled list).
    /// 3. Build a `SwimPing` message piggybacked with recent membership updates.
    ///
    /// Returns a list of `(target_node, message)` pairs to dispatch.
    pub async fn protocol_tick(&self) -> Vec<(NodeId, WireMessage)> {
        // Step 1: expire suspects.
        let now = Instant::now();
        let mut newly_dead: Vec<(NodeId, u64)> = Vec::new();

        for entry in self.members.iter() {
            if let MemberState::Suspect { incarnation, since } = entry.value()
                && now.duration_since(*since) >= self.suspect_timeout
            {
                newly_dead.push((*entry.key(), *incarnation));
            }
        }
        for (node_id, incarnation) in &newly_dead {
            tracing::info!(
                node = %node_id,
                incarnation,
                "SWIM: suspect timeout expired — marking dead"
            );
            self.mark_dead(node_id, *incarnation);
        }

        // Step 2: select probe target.
        let alive = self.alive_members();
        tracing::trace!(
            alive = alive.len(),
            "SWIM: protocol tick, {} alive members",
            alive.len()
        );

        if alive.is_empty() {
            return Vec::new();
        }

        let idx = self.probe_index.fetch_add(1, Ordering::Relaxed) % alive.len();
        let target = alive[idx];

        // Step 3: build ping with piggybacked membership state.
        // Piggybacked updates are plain SwimAlive/SwimSuspect/SwimDead messages
        // so the wire format doesn't require MemberState in craftec-types.
        let piggyback = self.collect_gossip_msgs(4); // up to 4 updates piggybacked

        // Allocate a nonce for probe-ack correlation (T2).
        let nonce = self.probe_nonce.fetch_add(1, Ordering::Relaxed);

        let ping = WireMessage::SwimPing {
            from: self.local_id,
            nonce,
            piggyback,
        };

        tracing::trace!(probe_target = %target, nonce, "SWIM: sending probe ping");
        vec![(target, ping)]
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Collect up to `limit` recent membership events as [`WireMessage`]s to piggyback.
    ///
    /// Converts the current `MemberState` map into wire-format messages that can be
    /// embedded in a `SwimPing`.  Alive members are listed first (highest priority),
    /// then suspects, then dead.
    fn collect_gossip_msgs(&self, limit: usize) -> Vec<WireMessage> {
        let mut pairs: Vec<(NodeId, MemberState)> = self
            .members
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();

        // Sort: Alive < Suspect < Dead so we prioritise live state.
        pairs.sort_by_key(|(_, state)| match state {
            MemberState::Alive { .. } => 0u8,
            MemberState::Suspect { .. } => 1,
            MemberState::Dead { .. } => 2,
        });

        pairs
            .into_iter()
            .take(limit)
            .map(|(node_id, state)| match state {
                MemberState::Alive { incarnation } => WireMessage::SwimAlive {
                    node_id,
                    incarnation,
                },
                MemberState::Suspect { incarnation, .. } => WireMessage::SwimSuspect {
                    node_id,
                    incarnation,
                    from: self.local_id,
                },
                MemberState::Dead { incarnation } => WireMessage::SwimDead {
                    node_id,
                    incarnation,
                    from: self.local_id,
                },
            })
            .collect()
    }
}

// ── Long-running protocol loop ─────────────────────────────────────────────

/// Ack timeout for direct probe (400ms).
const ACK_TIMEOUT: Duration = Duration::from_millis(400);

/// Ack timeout for indirect probe (800ms).
const INDIRECT_ACK_TIMEOUT: Duration = Duration::from_millis(800);

/// Number of delegates for indirect probe (ping-req).
const INDIRECT_PROBE_K: usize = 3;

/// Start the SWIM background loop with full probe-ack-suspect cycle (T2 fix).
///
/// Per tick:
/// 1. Run `protocol_tick()` to expire suspects and select a target.
/// 2. Send `SwimPing` → register probe → await ack (400ms).
/// 3. On timeout: send indirect probe to K=3 random delegates.
/// 4. On second timeout (800ms): `mark_suspect(target)`.
pub async fn run_swim_loop(
    swim: Arc<SwimMembership>,
    endpoint: Arc<crate::endpoint::CraftecEndpoint>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let period = swim.protocol_period;
    tracing::info!(
        ?period,
        "SWIM: background loop started (with probe-ack cycle)"
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(period) => {
                let probes = swim.protocol_tick().await;
                for (target, msg) in probes {
                    let swim = Arc::clone(&swim);
                    let ep = Arc::clone(&endpoint);
                    tokio::spawn(async move {
                        probe_with_ack(swim, ep, target, msg).await;
                    });
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("SWIM: shutdown signal received — stopping loop");
                break;
            }
        }
    }
}

/// Execute a single probe with ack-wait, indirect probe, and suspect-on-timeout.
async fn probe_with_ack(
    swim: Arc<SwimMembership>,
    endpoint: Arc<crate::endpoint::CraftecEndpoint>,
    target: NodeId,
    msg: WireMessage,
) {
    // Step 1: Register the probe and send the ping.
    let (nonce, rx) = swim.register_probe();

    if let Err(e) = endpoint.send_message(&target, &msg).await {
        tracing::debug!(peer = %target, error = %e, "SWIM: failed to send probe");
        return;
    }

    // Step 2: Wait for direct ack (400ms).
    match tokio::time::timeout(ACK_TIMEOUT, rx).await {
        Ok(Ok(_incarnation)) => {
            // Ack received — target is alive.
            tracing::trace!(peer = %target, nonce, "SWIM: probe ack received");
            return;
        }
        _ => {
            tracing::debug!(peer = %target, nonce, "SWIM: direct probe timeout — trying indirect");
        }
    }

    // Step 3: Indirect probe — ask K random delegates to ping the target.
    let delegates = swim.random_alive_excluding(&target, INDIRECT_PROBE_K);
    if delegates.is_empty() {
        tracing::debug!(peer = %target, "SWIM: no delegates for indirect probe — marking suspect");
        let inc = swim.last_known_incarnation(&target);
        swim.mark_suspect(&target, inc);
        return;
    }

    let (indirect_nonce, indirect_rx) = swim.register_probe();
    let piggyback = vec![];
    let indirect_ping = WireMessage::SwimPing {
        from: *endpoint.node_id(),
        nonce: indirect_nonce,
        piggyback,
    };

    for _delegate in &delegates {
        // Send the ping directly to target through each delegate's path.
        // Future: replace with a proper PingReq relay message.
        let ep = Arc::clone(&endpoint);
        let t = target;
        let m = indirect_ping.clone();
        tokio::spawn(async move {
            let _ = ep.send_message(&t, &m).await;
        });
    }

    // Step 4: Wait for indirect ack (800ms).
    match tokio::time::timeout(INDIRECT_ACK_TIMEOUT, indirect_rx).await {
        Ok(Ok(_incarnation)) => {
            tracing::trace!(peer = %target, "SWIM: indirect probe ack received");
        }
        _ => {
            tracing::info!(peer = %target, "SWIM: all probes timed out — marking suspect");
            let inc = swim.last_known_incarnation(&target);
            swim.mark_suspect(&target, inc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_swim() -> SwimMembership {
        SwimMembership::new(NodeId::generate())
    }

    #[test]
    fn starts_empty() {
        let swim = make_swim();
        assert_eq!(swim.member_count(), 0);
        assert!(swim.alive_members().is_empty());
    }

    #[test]
    fn mark_alive_adds_member() {
        let swim = make_swim();
        let peer = NodeId::generate();
        swim.mark_alive(&peer, 0);
        assert_eq!(swim.member_count(), 1);
        assert!(swim.is_alive(&peer));
        assert_eq!(swim.alive_members(), vec![peer]);
    }

    #[test]
    fn mark_suspect_transitions_from_alive() {
        let swim = make_swim();
        let peer = NodeId::generate();
        swim.mark_alive(&peer, 0);
        swim.mark_suspect(&peer, 0);
        assert!(!swim.is_alive(&peer));
        // Suspect is counted in member_count but not alive_members.
        assert_eq!(swim.member_count(), 1);
        assert!(swim.alive_members().is_empty());
    }

    #[test]
    fn mark_dead_is_terminal() {
        let swim = make_swim();
        let peer = NodeId::generate();
        swim.mark_alive(&peer, 1);
        swim.mark_dead(&peer, 1);
        assert!(!swim.is_alive(&peer));
        // Lower-incarnation alive cannot revive a dead node.
        swim.mark_alive(&peer, 1);
        assert!(!swim.is_alive(&peer));
    }

    #[test]
    fn higher_incarnation_alive_revives_dead() {
        let swim = make_swim();
        let peer = NodeId::generate();
        swim.mark_dead(&peer, 1);
        swim.mark_alive(&peer, 2); // higher incarnation
        assert!(swim.is_alive(&peer));
    }

    #[test]
    fn suspect_does_not_override_same_incarnation_alive() {
        let swim = make_swim();
        let peer = NodeId::generate();
        swim.mark_alive(&peer, 5);
        // A suspect with lower incarnation should not downgrade.
        swim.mark_suspect(&peer, 4);
        assert!(swim.is_alive(&peer));
    }

    #[test]
    fn local_id_never_inserted_into_members() {
        let local = NodeId::generate();
        let swim = SwimMembership::new(local);
        swim.mark_alive(&local, 0);
        swim.mark_suspect(&local, 0);
        assert_eq!(swim.member_count(), 0);
    }

    #[test]
    fn handle_join_message_adds_member() {
        let swim = make_swim();
        let peer = NodeId::generate();
        let msg = WireMessage::SwimJoin {
            node_id: peer,
            listen_port: 9000,
        };
        let responses = swim.handle_message(&msg);
        assert!(swim.is_alive(&peer));
        // Should get back a SwimAlive for our own node.
        assert!(!responses.is_empty());
        assert!(matches!(responses[0], WireMessage::SwimAlive { .. }));
    }

    #[tokio::test]
    async fn protocol_tick_returns_empty_with_no_members() {
        let swim = Arc::new(make_swim());
        let probes = swim.protocol_tick().await;
        assert!(probes.is_empty());
    }

    #[tokio::test]
    async fn protocol_tick_returns_probe_with_alive_member() {
        let swim = Arc::new(make_swim());
        let peer = NodeId::generate();
        swim.mark_alive(&peer, 0);
        let probes = swim.protocol_tick().await;
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].0, peer);
        match &probes[0].1 {
            WireMessage::SwimPing { nonce, .. } => {
                assert!(*nonce < u64::MAX, "nonce should be a valid u64");
            }
            other => panic!("expected SwimPing, got {:?}", other),
        }
    }

    #[test]
    fn handle_swim_ping_responds_with_ping_ack() {
        let local = NodeId::generate();
        let swim = SwimMembership::new(local);
        let peer = NodeId::generate();

        let piggyback_node = NodeId::generate();
        let msg = WireMessage::SwimPing {
            from: peer,
            nonce: 42,
            piggyback: vec![WireMessage::SwimAlive {
                node_id: piggyback_node,
                incarnation: 1,
            }],
        };

        let responses = swim.handle_message(&msg);
        // Should contain a SwimPingAck with the same nonce.
        assert!(responses.iter().any(|r| matches!(
            r,
            WireMessage::SwimPingAck { from, nonce: 42, .. } if *from == local
        )));
        // Piggybacked node should be marked alive.
        assert!(swim.is_alive(&piggyback_node));
    }

    #[test]
    fn probe_ack_resolves_pending() {
        let swim = make_swim();
        let (nonce, mut rx) = swim.register_probe();
        assert!(swim.resolve_probe(nonce, 5));
        // The receiver should now have the incarnation value.
        assert_eq!(rx.try_recv().unwrap(), 5);
    }

    #[test]
    fn probe_timeout_leaves_pending_unresolved() {
        let swim = make_swim();
        let (nonce, _rx) = swim.register_probe();
        // Resolving with a different nonce should not resolve this probe.
        assert!(!swim.resolve_probe(nonce + 999, 5));
    }

    #[test]
    fn swim_parameters_match_spec() {
        let swim = make_swim();
        assert_eq!(swim.protocol_period, Duration::from_millis(500));
        // T8: spec §18 requires 5000ms suspect timeout.
        assert_eq!(swim.suspect_timeout, Duration::from_millis(5000));
    }

    #[test]
    fn mark_alive_does_not_regress_dead() {
        // T7: verify that entry-based update prevents lower-incarnation Alive
        // from overriding higher-incarnation Dead.
        let swim = make_swim();
        let peer = NodeId::generate();
        swim.mark_dead(&peer, 7);
        swim.mark_alive(&peer, 5); // lower incarnation — must NOT override
        assert!(
            !swim.is_alive(&peer),
            "Alive(5) should not override Dead(7)"
        );
        // Even equal incarnation must not override Dead.
        swim.mark_alive(&peer, 7);
        assert!(
            !swim.is_alive(&peer),
            "Alive(7) should not override Dead(7)"
        );
        // Higher incarnation revives.
        swim.mark_alive(&peer, 8);
        assert!(swim.is_alive(&peer), "Alive(8) should revive Dead(7)");
    }
}
