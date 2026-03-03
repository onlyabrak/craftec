//! `craftec-vfs` — CID-VFS: content-addressed virtual file system for SQLite.
//!
//! CID-VFS bridges the SQLite VFS abstraction and CraftOBJ, Craftec's
//! content-addressed object store.  Instead of writing to a local file, every
//! 16 KiB SQLite page is:
//!
//! 1. **BLAKE3-hashed** to produce a *content identifier* (CID).
//! 2. **Stored** in CraftOBJ under that CID (append-only, deduplication free).
//! 3. **Indexed** in the page index: `page_number → CID`.
//! 4. **Root CID** = BLAKE3 of the serialised page index, representing the
//!    complete database state at a point in time.
//!
//! ## Key properties
//! - **Snapshot isolation** — pin a root CID at query start; reads always see
//!   a consistent state.
//! - **No WAL** — CraftOBJ is append-only; old pages remain until a future GC
//!   agent reclaims them.
//! - **Hot page cache** — LRU cache keyed by `(root_cid, page_num)` for ~0.013 ms
//!   p50 latency on hits.
//! - **Single-writer per identity** — enforced by the CraftSQL layer above.
//!
//! ## Crate layout
//! | Module | Responsibility |
//! |---|---|
//! | [`vfs`] | Core [`CidVfs`] type: read/write/commit/snapshot |
//! | [`page_index`] | `page_number → CID` mapping |
//! | [`page_cache`] | LRU hot-page cache |
//! | [`snapshot`] | Pinned snapshot for read isolation |
//! | [`error`] | [`VfsError`] enum and [`Result`] alias |

pub mod error;
pub mod page_cache;
pub mod page_index;
pub mod snapshot;
pub mod vfs;

// Convenience re-exports.
pub use error::{Result, VfsError};
pub use page_cache::PageCache;
pub use page_index::PageIndex;
pub use snapshot::Snapshot;
pub use vfs::{CidVfs, DEFAULT_PAGE_SIZE};
