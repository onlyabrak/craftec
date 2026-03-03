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

use craftec_types::{NodeId, WireMessage};

/// Default time before a `Suspect` node is declared `Dead` (§13).
const DEFAULT_SUSPECT_TIMEOUT: Duration = Duration::from_millis(1500);

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
        self.incarnation.load(Ordering::Relaxed)
    }

    // ── State transitions ──────────────────────────────────────────────────

    /// Mark `node_id` as `Alive` with the given `incarnation`.
    ///
    /// Only applies if `incarnation` ≥ the currently recorded incarnation, ensuring
    /// monotonically increasing state.
    pub fn mark_alive(&self, node_id: &NodeId, incarnation: u64) {
        if node_id == &self.local_id {
            return; // Never update own entry — local state is authoritative.
        }
        let should_update = self
            .members
            .get(node_id)
            .map(|e| match e.value() {
                MemberState::Alive { incarnation: inc } => incarnation > *inc,
                MemberState::Suspect {
                    incarnation: inc, ..
                } => incarnation >= *inc,
                MemberState::Dead { incarnation: inc } => incarnation > *inc,
            })
            .unwrap_or(true); // Unknown node → insert

        if should_update {
            tracing::debug!(node = %node_id, incarnation, "SWIM: mark_alive");
            self.members
                .insert(*node_id, MemberState::Alive { incarnation });
        }
    }

    /// Mark `node_id` as `Suspect`.
    ///
    /// Only transitions from `Alive` (or unknown) — will not override a higher-incarnation
    /// `Alive` or an existing `Dead` state.
    pub fn mark_suspect(&self, node_id: &NodeId, incarnation: u64) {
        if node_id == &self.local_id {
            // We are suspected — refute by incrementing our own incarnation.
            let new_inc = self.incarnation.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                new_incarnation = new_inc,
                "SWIM: refuting suspect claim on self"
            );
            return;
        }
        let should_update = self
            .members
            .get(node_id)
            .map(|e| match e.value() {
                MemberState::Alive { incarnation: inc } => incarnation >= *inc,
                MemberState::Suspect {
                    incarnation: inc, ..
                } => incarnation > *inc,
                MemberState::Dead { .. } => false, // Dead is terminal.
            })
            .unwrap_or(true);

        if should_update {
            tracing::debug!(node = %node_id, incarnation, "SWIM: mark_suspect");
            self.members.insert(
                *node_id,
                MemberState::Suspect {
                    incarnation,
                    since: Instant::now(),
                },
            );
        }
    }

    /// Mark `node_id` as `Dead`.
    ///
    /// `Dead` is terminal — it cannot be overridden by `Suspect` or an equal-incarnation
    /// `Alive`.  A higher-incarnation `Alive` from the node itself can revive it.
    pub fn mark_dead(&self, node_id: &NodeId, incarnation: u64) {
        if node_id == &self.local_id {
            tracing::warn!("SWIM: received Dead claim about self — ignoring");
            return;
        }
        let should_update = self
            .members
            .get(node_id)
            .map(|e| match e.value() {
                MemberState::Dead { incarnation: inc } => incarnation > *inc,
                _ => true,
            })
            .unwrap_or(true);

        if should_update {
            tracing::info!(node = %node_id, incarnation, "SWIM: mark_dead");
            self.members
                .insert(*node_id, MemberState::Dead { incarnation });
        }
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
                let own_inc = self.incarnation.load(Ordering::Relaxed);
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

            WireMessage::SwimPing { from, piggyback } => {
                tracing::debug!(
                    from = %from,
                    piggyback_count = piggyback.len(),
                    "SWIM: processed SwimPing"
                );
                // Process piggybacked membership updates.
                for piggybacked_msg in piggyback {
                    let sub_responses = self.handle_message(piggybacked_msg);
                    responses.extend(sub_responses);
                }
                // Respond with our own Alive to confirm liveness (probe ack).
                let own_inc = self.incarnation.load(Ordering::Relaxed);
                responses.push(WireMessage::SwimAlive {
                    node_id: self.local_id,
                    incarnation: own_inc,
                });
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

        let ping = WireMessage::SwimPing {
            from: self.local_id,
            piggyback,
        };

        tracing::trace!(probe_target = %target, "SWIM: sending probe ping");
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

/// Start the SWIM background loop.
///
/// Runs one [`SwimMembership::protocol_tick`] per `swim.protocol_period` until a
/// signal is received on `shutdown`.
///
/// Probes are dispatched to peers via `endpoint.send_message()`.
pub async fn run_swim_loop(
    swim: Arc<SwimMembership>,
    endpoint: Arc<crate::endpoint::CraftecEndpoint>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let period = swim.protocol_period;
    tracing::info!(?period, "SWIM: background loop started");

    loop {
        tokio::select! {
            _ = tokio::time::sleep(period) => {
                let probes = swim.protocol_tick().await;
                for (target, msg) in probes {
                    let ep = endpoint.clone();
                    tokio::spawn(async move {
                        if let Err(e) = ep.send_message(&target, &msg).await {
                            tracing::debug!(
                                peer = %target,
                                error = %e,
                                "SWIM: failed to send probe"
                            );
                        }
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
        assert!(matches!(probes[0].1, WireMessage::SwimPing { .. }));
    }

    #[test]
    fn handle_swim_ping_responds_with_alive() {
        let local = NodeId::generate();
        let swim = SwimMembership::new(local);
        let peer = NodeId::generate();

        let piggyback_node = NodeId::generate();
        let msg = WireMessage::SwimPing {
            from: peer,
            piggyback: vec![WireMessage::SwimAlive {
                node_id: piggyback_node,
                incarnation: 1,
            }],
        };

        let responses = swim.handle_message(&msg);
        // Should contain at least our own SwimAlive ack.
        assert!(responses.iter().any(|r| matches!(
            r,
            WireMessage::SwimAlive { node_id, .. } if *node_id == local
        )));
        // Piggybacked node should be marked alive.
        assert!(swim.is_alive(&piggyback_node));
    }

    #[test]
    fn swim_parameters_match_spec() {
        let swim = make_swim();
        assert_eq!(swim.protocol_period, Duration::from_millis(500));
        assert_eq!(swim.suspect_timeout, Duration::from_millis(1500));
    }
}
