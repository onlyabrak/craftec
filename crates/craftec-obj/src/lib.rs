//! # craftec-obj — CraftOBJ Content-Addressed Storage Layer
//!
//! `craftec-obj` implements the local storage tier of the CraftOBJ distributed
//! storage system. It provides a **content-addressed, immutable object store**
//! backed by the local filesystem.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                  ContentAddressedStore                   │
//! │                                                         │
//! │  ┌──────────────┐  ┌──────────────┐  ┌─────────────┐  │
//! │  │  ObjectCache  │  │CidBloomFilter│  │ Filesystem  │  │
//! │  │  (LRU, RAM)  │  │  (in-memory) │  │(256 shards) │  │
//! │  └──────┬───────┘  └──────┬───────┘  └──────┬──────┘  │
//! │         │  cache hit      │  bloom miss      │         │
//! │   get ──┤                 ├──────────────────┤         │
//! │         │  cache miss ────┘  bloom hit       │         │
//! │         │                                    │  read + │
//! │         │                                    │  verify │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! ### Key design decisions
//!
//! - **CID = filename**: no separate database index. The filesystem IS the
//!   index. Listing all stored objects is a directory walk.
//! - **Directory sharding**: objects are stored under `<base>/<first-2-hex>/`
//!   (256 subdirectories), keeping individual directory sizes manageable even
//!   with millions of objects.
//! - **Bloom filter**: a probabilistic membership test that rules out disk I/O
//!   for CIDs that are _definitely_ not present.
//! - **LRU cache**: recently-accessed objects are kept in RAM for fast
//!   repeated reads.
//! - **Immutability**: objects are never modified after the initial write.
//!   Content-addressing makes mutation meaningless — changing bytes changes
//!   the CID.
//! - **Integrity on every read**: every disk read is verified with BLAKE3.
//!   Silent corruption is always detected.
//! - **Atomic writes**: objects are written to a `.tmp` file and renamed into
//!   place, so concurrent readers never see partial data.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use craftec_obj::store::ContentAddressedStore;
//! use std::path::Path;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let store = ContentAddressedStore::new(Path::new("/var/craftobj"), 4096)?;
//!
//!     // Write an object.
//!     let cid = store.put(b"hello world").await?;
//!     println!("stored as {cid}");
//!
//!     // Read it back (always verified with BLAKE3).
//!     let bytes = store.get(&cid).await?;
//!     assert_eq!(bytes.as_deref(), Some(b"hello world".as_ref()));
//!
//!     // Check membership without reading the full object.
//!     assert!(store.contains(&cid).await);
//!
//!     // Remove it.
//!     store.delete(&cid).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`store`] | [`ContentAddressedStore`] — the main public type |
//! | [`cache`] | [`ObjectCache`] — LRU cache wrapper |
//! | [`bloom`] | [`CidBloomFilter`] — bloom filter wrapper |
//! | [`shard`] | Directory sharding helpers |
//! | [`error`] | [`ObjError`] and [`Result`] type alias |

pub mod bloom;
pub mod cache;
pub mod error;
pub mod shard;
pub mod store;

// Re-export the most commonly used types at the crate root for convenience.
pub use error::{ObjError, Result};
pub use store::{ContentAddressedStore, StoreMetrics};
