//! Agent lifecycle types for CraftCOM.
//!
//! A Craftec *agent* is a network-owned WASM program managed by the
//! [`ProgramScheduler`](crate::scheduler::ProgramScheduler).  Agents execute
//! inside a Wasmtime sandbox with fuel-based limits and access to Craftec host
//! functions.
//!
//! ## Built-in agent types
//! The Craftec kernel ships a set of built-in agents that implement core
//! network policies.  Third-party agents can be deployed by any node that
//! holds the correct signing key.
//!
//! | Agent | Description |
//! |---|---|
//! | [`AgentKind::LocalEviction`] | Evicts cold pages from CraftOBJ to free disk |
//! | [`AgentKind::ReputationScoring`] | Scores peers based on availability and latency |
//! | [`AgentKind::LoadBalancing`] | Routes requests to least-loaded nodes |
//! | [`AgentKind::DegradationPolicy`] | Detects degraded nodes and adjusts replication |
//! | [`AgentKind::SchemaMigration`] | Applies CraftSQL schema migrations safely |
//! | [`AgentKind::Custom`] | Third-party agent (identified by CID) |

use craftec_types::Cid;

/// Well-known built-in agent types.
///
/// These agents are compiled into the node binary as WASM blobs and
/// auto-started by the [`ProgramScheduler`](crate::scheduler::ProgramScheduler)
/// on node boot (subject to node configuration).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentKind {
    /// Evicts cold / unreferenced CraftOBJ pages to reclaim local storage.
    ///
    /// Runs on a configurable schedule (default: every 10 minutes).
    LocalEviction,

    /// Scores the reputation of connected peers based on:
    /// - Historical request success rate.
    /// - Average latency.
    /// - Content availability (did they serve what they claimed?).
    ReputationScoring,

    /// Balances outbound CraftOBJ requests across peers weighted by reputation
    /// and current load.
    LoadBalancing,

    /// Monitors replication health and triggers re-replication when peer
    /// availability drops below the target redundancy factor.
    DegradationPolicy,

    /// Applies pending CraftSQL schema migrations safely, ensuring no readers
    /// see a partial schema state.
    SchemaMigration,

    /// A third-party or application-level agent identified by its WASM CID.
    Custom(Cid),
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentKind::LocalEviction => write!(f, "LocalEviction"),
            AgentKind::ReputationScoring => write!(f, "ReputationScoring"),
            AgentKind::LoadBalancing => write!(f, "LoadBalancing"),
            AgentKind::DegradationPolicy => write!(f, "DegradationPolicy"),
            AgentKind::SchemaMigration => write!(f, "SchemaMigration"),
            AgentKind::Custom(cid) => write!(f, "Custom({cid})"),
        }
    }
}

/// A running (or recently stopped) network-owned WASM agent.
///
/// Agents are created by the [`ProgramScheduler`](crate::scheduler::ProgramScheduler)
/// when a WASM program transitions to the `Running` state.  The `wasm_cid`
/// uniquely identifies the program binary; the `name` is a human-readable
/// label for observability.
#[derive(Debug, Clone)]
pub struct Agent {
    /// CID of the WASM binary that this agent is running.
    pub wasm_cid: Cid,
    /// Human-readable name (e.g., "LocalEviction", or a custom label).
    pub name: String,
    /// Logical agent type.
    pub kind: AgentKind,
}

impl Agent {
    /// Create a new [`Agent`] descriptor.
    pub fn new(wasm_cid: Cid, name: impl Into<String>, kind: AgentKind) -> Self {
        Self {
            wasm_cid,
            name: name.into(),
            kind,
        }
    }

    /// Return the canonical [`Agent`] for the `LocalEviction` built-in.
    pub fn local_eviction(wasm_cid: Cid) -> Self {
        Self::new(wasm_cid, "LocalEviction", AgentKind::LocalEviction)
    }

    /// Return the canonical [`Agent`] for the `ReputationScoring` built-in.
    pub fn reputation_scoring(wasm_cid: Cid) -> Self {
        Self::new(wasm_cid, "ReputationScoring", AgentKind::ReputationScoring)
    }

    /// Return the canonical [`Agent`] for the `LoadBalancing` built-in.
    pub fn load_balancing(wasm_cid: Cid) -> Self {
        Self::new(wasm_cid, "LoadBalancing", AgentKind::LoadBalancing)
    }

    /// Return the canonical [`Agent`] for the `DegradationPolicy` built-in.
    pub fn degradation_policy(wasm_cid: Cid) -> Self {
        Self::new(wasm_cid, "DegradationPolicy", AgentKind::DegradationPolicy)
    }

    /// Return the canonical [`Agent`] for the `SchemaMigration` built-in.
    pub fn schema_migration(wasm_cid: Cid) -> Self {
        Self::new(wasm_cid, "SchemaMigration", AgentKind::SchemaMigration)
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Agent({}, wasm={})", self.name, self.wasm_cid)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_types::Cid;

    fn cid(seed: u8) -> Cid {
        Cid::from_bytes([seed; 32])
    }

    #[test]
    fn built_in_constructors_produce_correct_kind() {
        assert_eq!(Agent::local_eviction(cid(0)).kind, AgentKind::LocalEviction);
        assert_eq!(
            Agent::reputation_scoring(cid(1)).kind,
            AgentKind::ReputationScoring
        );
        assert_eq!(Agent::load_balancing(cid(2)).kind, AgentKind::LoadBalancing);
        assert_eq!(
            Agent::degradation_policy(cid(3)).kind,
            AgentKind::DegradationPolicy
        );
        assert_eq!(
            Agent::schema_migration(cid(4)).kind,
            AgentKind::SchemaMigration
        );
    }

    #[test]
    fn custom_agent_kind_display() {
        let kind = AgentKind::Custom(cid(0xFF));
        let s = format!("{kind}");
        assert!(s.starts_with("Custom("));
    }

    #[test]
    fn agent_display_includes_name_and_cid() {
        let agent = Agent::local_eviction(cid(0x01));
        let s = format!("{agent}");
        assert!(s.contains("LocalEviction"));
        assert!(s.contains("wasm="));
    }
}
