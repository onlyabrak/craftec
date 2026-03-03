//! `craftec-types` — foundational type definitions for the Craftec P2P storage system.
//!
//! This crate provides all shared data types used across the Craftec codebase:
//! content identifiers, piece structures, node identity, wire protocol messages,
//! error types, node configuration, and event bus types.
//!
//! All other Craftec crates depend on this crate.

pub mod cid;
pub mod config;
pub mod error;
pub mod event;
pub mod identity;
pub mod piece;
pub mod wire;

// Convenience re-exports of the most commonly used types.
pub use cid::{Cid, CID_SIZE};
pub use config::NodeConfig;
pub use error::{CraftecError, Result};
pub use event::Event;
pub use identity::{NodeId, NodeKeypair, Signature};
pub use piece::{CodedPiece, HomMAC, PieceId, PieceIndex, GF_ORDER, K_DEFAULT, PAGE_SIZE};
pub use wire::{decode, encode, WireMessage};
