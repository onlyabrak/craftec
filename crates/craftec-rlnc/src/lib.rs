//! `craftec-rlnc` — Random Linear Network Coding engine over GF(2⁸).
//!
//! This crate provides the full RLNC pipeline for the Craftec distributed
//! storage system:
//!
//! | Component         | Description                                              |
//! |-------------------|----------------------------------------------------------|
//! | [`gf256`]         | GF(2⁸) arithmetic: add, mul, div, inv, fused-mul-add    |
//! | [`encoder`]       | Split data → K source pieces → unlimited coded pieces    |
//! | [`decoder`]       | Collect ≥ K independent pieces → Gaussian elim → data   |
//! | [`recoder`]       | Combine coded pieces into new ones without decoding      |
//! | [`engine`]        | Async façade with Semaphore(8) concurrency limit         |
//! | [`error`]         | [`RlncError`] and [`Result`] alias                       |
//!
//! # Quick start
//!
//! ```rust,no_run
//! use craftec_rlnc::engine::RlncEngine;
//!
//! #[tokio::main]
//! async fn main() {
//!     let engine = RlncEngine::new();
//!
//!     // Encode 32 KiB of data into coded pieces.
//!     let data = vec![0u8; 32 * 1024];
//!     let pieces = engine.encode(&data, 32).await.unwrap();
//!
//!     // Decode back to the original.
//!     let piece_size = pieces[0].data.len();
//!     let recovered = engine.decode(32, piece_size, &pieces).await.unwrap();
//!     assert_eq!(&recovered[..data.len()], data.as_slice());
//! }
//! ```
//!
//! # Field parameters
//!
//! * Irreducible polynomial: `p(x) = x⁸ + x⁴ + x³ + x + 1` (AES, 0x11B)
//! * Primitive generator: `g = 0x03`
//! * Default generation size: `K = 32`
//! * Redundancy formula: `redundancy(K) = 2.0 + 16.0 / K`
//!
//! # Concurrency model
//!
//! All public operations are exposed as `async fn` via [`RlncEngine`].
//! The engine holds a [`tokio::sync::Semaphore`] with 8 permits, so at most
//! 8 RLNC operations run concurrently across the node.

#![warn(missing_docs)]

pub mod decoder;
pub mod encoder;
pub mod engine;
pub mod error;
pub mod gf256;
pub mod recoder;

// Top-level re-exports for ergonomic use.
pub use decoder::RlncDecoder;
pub use encoder::RlncEncoder;
pub use engine::{MetricsSnapshot, RlncEngine, RlncMetrics};
pub use error::{Result, RlncError};
pub use recoder::RlncRecoder;
