//! # Directory sharding for CraftOBJ
//!
//! CraftOBJ uses the **filesystem as its index**: each object is a flat file
//! whose name is the hex-encoded CID (64 ASCII chars). To avoid performance
//! problems with directories that contain millions of entries, objects are
//! spread across 256 subdirectories named `00` through `ff` — the first two
//! hex characters of the CID.
//!
//! ## Layout on disk
//!
//! ```text
//! <base_dir>/
//!   00/
//!     00a1b2c3…  ← full 64-char hex CID filename, 32-byte content CID
//!   01/
//!     01dead…
//!   …
//!   ff/
//!     ffbeef…
//! ```
//!
//! This spreads objects uniformly across 256 subdirectories (BLAKE3 output is
//! uniformly distributed), so a store with 1 million objects has ~3,900 files
//! per directory — a comfortable operating range for most filesystems.
//!
//! ## No database required
//!
//! Listing all stored CIDs is simply a directory walk: iterate the 256 shard
//! directories and parse each filename back into a [`Cid`]. The filesystem's
//! own B-tree index (or hash table) *is* the index.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use craftec_types::Cid;
use tracing::{debug, trace, warn};

use crate::error::{ObjError, Result};

/// Compute the full filesystem path for an object identified by `cid`.
///
/// The path is `<base>/<first-2-hex-chars>/<full-64-hex-chars>`.
///
/// # Examples
///
/// ```rust,ignore
/// let path = shard_path(Path::new("/var/craftobj"), &cid);
/// // → PathBuf from "/var/craftobj/a3/a3f9b2…"
/// ```
pub fn shard_path(base: &Path, cid: &Cid) -> PathBuf {
    let hex = cid.to_string();
    // Safety: hex is always exactly 64 lowercase ASCII chars.
    let shard_dir = &hex[..2];
    base.join(shard_dir).join(&hex)
}

/// Ensure all 256 shard subdirectories (`00`..`ff`) exist under `base`.
///
/// This is called once during store initialisation. It is idempotent — calling
/// it on an already-initialised store is a no-op.
///
/// # Errors
///
/// Returns an [`ObjError::IoError`] if any directory cannot be created, e.g.
/// due to permission errors.
pub fn ensure_shard_dirs(base: &Path) -> Result<()> {
    debug!(base = ?base, "CraftOBJ shard: ensuring 256 shard directories");
    for i in 0u8..=255 {
        let name = format!("{:02x}", i);
        let path = base.join(&name);
        std::fs::create_dir_all(&path).map_err(|e| {
            warn!(path = ?path, error = %e, "CraftOBJ shard: failed to create shard dir");
            ObjError::IoError(e)
        })?;
    }
    debug!(base = ?base, "CraftOBJ shard: all 256 shard directories ready");
    Ok(())
}

/// Walk all 256 shard directories under `base` and collect every valid CID.
///
/// A file is considered a valid object entry if its name is a 64-character
/// lowercase hex string that successfully parses as a [`Cid`]. Entries with
/// unexpected names are skipped with a warning.
///
/// # Errors
///
/// Returns an [`ObjError::IoError`] if the base directory or any shard
/// directory cannot be read.
pub fn walk_shards(base: &Path) -> Result<Vec<Cid>> {
    trace!(base = ?base, "CraftOBJ shard: walking all shards");
    let mut cids = Vec::new();

    for i in 0u8..=255 {
        let shard_name = format!("{:02x}", i);
        let shard_dir = base.join(&shard_name);

        // Shard dirs may not exist on a freshly created store that hasn't been
        // fully initialised yet. Skip them gracefully.
        if !shard_dir.exists() {
            continue;
        }

        let read_dir = std::fs::read_dir(&shard_dir).map_err(ObjError::IoError)?;

        for entry in read_dir {
            let entry = entry.map_err(ObjError::IoError)?;
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();

            match Cid::from_str(&fname_str) {
                Ok(cid) => {
                    trace!(cid = %cid, "CraftOBJ shard: discovered object");
                    cids.push(cid);
                }
                Err(_) => {
                    warn!(
                        path = ?entry.path(),
                        name = %fname_str,
                        "CraftOBJ shard: skipping unexpected file in shard directory"
                    );
                }
            }
        }
    }

    debug!(base = ?base, count = cids.len(), "CraftOBJ shard: walk complete");
    Ok(cids)
}

/// Return the expected shard subdirectory name (first two hex chars) for a
/// given CID.
///
/// Useful for logging and diagnostics.
pub fn shard_prefix(cid: &Cid) -> String {
    cid.to_string()[..2].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_cid(data: &[u8]) -> Cid {
        Cid::from_data(data)
    }

    #[test]
    fn shard_path_structure() {
        let base = Path::new("/tmp/craftobj_test");
        let cid = make_cid(b"test object");
        let path = shard_path(base, &cid);

        // Should be: /tmp/craftobj_test / <2-char-shard> / <64-char-cid>
        let hex = cid.to_string();
        let shard = &hex[..2];
        let file = &hex;

        assert_eq!(
            path,
            PathBuf::from(format!("/tmp/craftobj_test/{}/{}", shard, file))
        );
    }

    #[test]
    fn shard_path_first_two_chars_match_subdir() {
        let base = Path::new("/base");
        let cid = make_cid(b"another test");
        let path = shard_path(base, &cid);
        let hex = cid.to_string();

        // Parent directory name == first two hex chars of CID
        let parent_name = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(parent_name, &hex[..2]);
    }

    #[test]
    fn ensure_shard_dirs_creates_256_dirs() {
        let dir = tempfile::tempdir().unwrap();
        ensure_shard_dirs(dir.path()).unwrap();

        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 256, "should create exactly 256 shard directories");
    }

    #[test]
    fn ensure_shard_dirs_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        ensure_shard_dirs(dir.path()).unwrap();
        ensure_shard_dirs(dir.path()).unwrap(); // must not error
    }

    #[test]
    fn walk_shards_empty() {
        let dir = tempfile::tempdir().unwrap();
        ensure_shard_dirs(dir.path()).unwrap();
        let cids = walk_shards(dir.path()).unwrap();
        assert!(cids.is_empty());
    }

    #[test]
    fn walk_shards_finds_objects() {
        let dir = tempfile::tempdir().unwrap();
        ensure_shard_dirs(dir.path()).unwrap();

        let items: &[&[u8]] = &[b"alpha", b"beta", b"gamma"];
        let mut expected: Vec<Cid> = Vec::new();

        for data in items {
            let cid = make_cid(data);
            let path = shard_path(dir.path(), &cid);
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(data).unwrap();
            expected.push(cid);
        }

        let mut found = walk_shards(dir.path()).unwrap();
        found.sort();
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn walk_shards_skips_invalid_filenames() {
        let dir = tempfile::tempdir().unwrap();
        ensure_shard_dirs(dir.path()).unwrap();

        // Plant a valid object.
        let cid = make_cid(b"valid");
        let valid_path = shard_path(dir.path(), &cid);
        std::fs::write(&valid_path, b"valid").unwrap();

        // Plant a junk file in the same shard.
        let hex = cid.to_string();
        let shard = &hex[..2];
        let junk_path = dir.path().join(shard).join("not_a_cid.tmp");
        std::fs::write(&junk_path, b"junk").unwrap();

        let found = walk_shards(dir.path()).unwrap();
        assert_eq!(found, vec![cid]);
    }

    #[test]
    fn shard_prefix_is_two_chars() {
        let cid = make_cid(b"any");
        let prefix = shard_prefix(&cid);
        assert_eq!(prefix.len(), 2);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn uniform_distribution_across_shards() {
        // With a large sample, every possible prefix should appear roughly
        // equally often. We just check the set has reasonable coverage.
        let count = 256;
        let prefixes: std::collections::HashSet<String> = (0u32..count as u32)
            .map(|i| {
                let cid = make_cid(&i.to_be_bytes());
                shard_prefix(&cid)
            })
            .collect();
        // At 256 samples with BLAKE3's uniform distribution, we expect ~161
        // unique prefixes (birthday problem). At least half should be unique.
        assert!(
            prefixes.len() > count / 2,
            "expected reasonable prefix spread, got {}",
            prefixes.len()
        );
    }
}
