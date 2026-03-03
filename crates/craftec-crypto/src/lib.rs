//! `craftec-crypto` — cryptographic utilities for the Craftec P2P storage
//! system.
//!
//! Provides:
//! - BLAKE3 hashing helpers ([`hash`])
//! - Ed25519 persistent keypair management ([`sign`])
//! - Homomorphic MAC for RLNC piece integrity ([`hommac`])

pub mod hash;
pub mod hommac;
pub mod sign;

// Convenience re-exports.
pub use hash::{hash_bytes, hash_page, merkle_root, verify_cid};
pub use hommac::{HomMacKey, combine_tags, compute_tag, verify_tag};
pub use sign::KeyStore;
