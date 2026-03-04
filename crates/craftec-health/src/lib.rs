//! `craftec-health` — continuous health scanning and self-healing repair for Craftec.
//!
//! This crate implements the background health layer that keeps stored content
//! durably available even as nodes join and leave the network.
//!
//! # Subsystems
//!
//! - **[`scanner`]**: The [`HealthScanner`] engine that cycles through 1% of all
//!   known CIDs per tick, identifies under-replicated pieces, and emits [`RepairRequest`]s.
//! - **[`coordinator`]**: The [`NaturalSelectionCoordinator`] that elects a repair
//!   coordinator for each CID using a deterministic ranking of available nodes.
//! - **[`repair`]**: The [`RepairExecutor`] that recodes from ≥2 locally-held coded pieces
//!   (without decoding!) using RLNC, and distributes the new piece to under-provisioned peers.
//! - **[`tracker`]**: The [`PieceTracker`] that maintains the live map of which nodes
//!   hold which pieces for which CIDs.
//! - **[`error`]**: The [`HealthError`] enum covering all health-layer failures.
//!
//! # Design principles
//!
//! 1. **Recode, never decode.** RLNC allows any node holding ≥2 coded pieces to
//!    create a new coded piece without ever recovering the original data.  This is
//!    the core property that makes self-healing P2P storage possible.
//!
//! 2. **1% per cycle, 100% coverage in 100 cycles.** `HealthScanner` processes
//!    `scan_percent` of all CIDs per tick, advancing a cursor through the sorted CID
//!    list.  At the default 1% rate the scanner visits every CID in 100 ticks.
//!
//! 3. **Natural Selection coordinator election.** The node with the highest uptime
//!    (then reputation, then lowest NodeId as tiebreaker) is elected repair coordinator
//!    for each CID.  No token economy, no randomness — deterministic, verifiable.
//!
//! 4. **Random piece distribution.** New recoded pieces are distributed to nodes that
//!    are missing pieces, chosen uniformly at random.  No XOR-distance routing.

pub mod coordinator;
pub mod error;
pub mod repair;
pub mod scanner;
pub mod tracker;

pub use coordinator::{NaturalSelectionCoordinator, NodeRanking};
pub use error::HealthError;
pub use repair::{RepairExecutor, RepairRequest};
pub use scanner::HealthScanner;
pub use tracker::{PieceHolder, PieceTracker};
