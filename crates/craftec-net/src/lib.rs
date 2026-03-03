//! `craftec-net` — P2P networking layer for the Craftec distributed storage system.
//!
//! This crate provides the core networking primitives for Craftec nodes:
//!
//! - **[`endpoint`]**: The main [`CraftecEndpoint`] wrapping `iroh::Endpoint` with
//!   ALPN-based protocol routing, bootstrap connectivity, and message dispatch.
//! - **[`swim`]**: A SWIM (Scalable Weakly-consistent Infection-style Membership)
//!   protocol implementation for O(log N) failure detection and membership dissemination.
//! - **[`connection`]**: The [`ConnectionHandler`] trait for application-layer protocol handlers.
//! - **[`pool`]**: The [`ConnectionPool`] managing live `iroh::Connection` handles with
//!   idle-timeout pruning.
//! - **[`dht`]**: The [`DhtProviders`] table mapping content CIDs to the nodes that hold them.
//! - **[`error`]**: The [`NetError`] enum covering all network-layer failures.
//!
//! ## ALPN protocol identifiers
//!
//! Two ALPN tokens are registered on every `CraftecEndpoint`:
//!
//! - `b"craftec/0.1"` — general Craftec RPC (wire messages, piece exchange).
//! - `b"craftec-swim/0.1"` — SWIM membership messages.
//!
//! Both share a single underlying `iroh::Endpoint`, enabling NAT traversal and relay
//! fallback for all Craftec protocols without extra sockets.

pub mod connection;
pub mod dht;
pub mod endpoint;
pub mod error;
pub mod pool;
pub mod swim;

pub use connection::ConnectionHandler;
pub use dht::DhtProviders;
pub use endpoint::{CraftecEndpoint, ALPN_CRAFTEC, ALPN_SWIM};
pub use error::NetError;
pub use pool::ConnectionPool;
pub use swim::SwimMembership;
