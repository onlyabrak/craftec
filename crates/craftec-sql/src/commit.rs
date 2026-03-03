//! CraftSQL commit flow.
//!
//! Documents and implements the synchronous steps of the write path that
//! occur within the database process before results are propagated to the
//! network.
//!
//! ## Full write path (11 steps)
//!
//! ### Synchronous steps (implemented here)
//! | # | Action |
//! |---|--------|
//! | 1 | Verify Ed25519 signature on the write message |
//! | 2 | Verify `writer == owner` (single-writer enforcement) |
//! | 3 | Compare-and-swap: check caller's `expected_root == current_root` |
//! | 4 | Execute the SQL mutation through CID-VFS |
//! | 5 | Flush dirty pages → CraftOBJ (BLAKE3 per page, via `store.put`) |
//! | 6 | Update page index (`page_num → CID`) |
//! | 7 | Compute new root CID = BLAKE3(serialised page index) |
//!
//! ### Asynchronous steps (handled by the caller / network layer)
//! | # | Action |
//! |---|--------|
//! | 8  | Broadcast new root CID to subscribed readers |
//! | 9  | Replicate changed objects via RLNC to redundancy targets |
//! | 10 | Update convenience SQL index for frontend discovery |
//! | 11 | Acknowledge to the writer with the new root CID |

use craftec_types::{Cid, NodeId};

use crate::error::{Result, SqlError};

/// Context captured at the start of a commit.
///
/// Passed through the commit pipeline so that each step can read immutable
/// inputs without taking locks on the database struct itself.
#[derive(Debug)]
pub struct CommitContext {
    /// The node executing the write.
    pub writer: NodeId,
    /// The SQL statement to execute.
    pub sql: String,
    /// Root CID the writer believes is current (compare-and-swap guard).
    pub expected_root: Option<Cid>,
}

/// Result of a successful commit.
#[derive(Debug, Clone)]
pub struct CommitResult {
    /// New root CID after the commit.
    pub new_root: Cid,
    /// Number of pages written to CraftOBJ during this commit.
    pub pages_written: usize,
}

/// Performs the synchronous CAS check (step 3 of the write path).
///
/// Compares `ctx.expected_root` against `current_root`.  If the caller
/// supplied `None` for `expected_root`, the check is skipped (first write).
///
/// # Errors
/// Returns [`SqlError::CasConflict`] when the roots differ.
pub fn check_cas(
    ctx: &CommitContext,
    current_root: Option<Cid>,
) -> Result<()> {
    match (ctx.expected_root, current_root) {
        (Some(expected), Some(current)) if expected != current => {
            Err(SqlError::CasConflict {
                expected: format!("{expected}"),
                actual: format!("{current}"),
            })
        }
        _ => Ok(()),
    }
}

/// Validates that the `writer` in `ctx` matches the `owner`.
///
/// This is the single-writer enforcement gate (step 2 of the write path).
///
/// # Errors
/// Returns [`SqlError::UnauthorizedWriter`] when they differ.
pub fn check_ownership(ctx: &CommitContext, owner: &NodeId) -> Result<()> {
    if &ctx.writer != owner {
        return Err(SqlError::UnauthorizedWriter {
            writer: format!("{}", ctx.writer),
            owner: format!("{owner}"),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use craftec_types::{Cid, NodeKeypair};

    fn cid(seed: u8) -> Cid {
        Cid::from_bytes([seed; 32])
    }

    fn make_ctx(writer: NodeId, expected_root: Option<Cid>) -> CommitContext {
        CommitContext {
            writer,
            sql: "INSERT INTO t VALUES (1)".into(),
            expected_root,
        }
    }

    #[test]
    fn cas_passes_when_roots_match() {
        let node = NodeKeypair::generate().node_id();
        let root = cid(0x01);
        let ctx = make_ctx(node, Some(root));
        assert!(check_cas(&ctx, Some(root)).is_ok());
    }

    #[test]
    fn cas_fails_when_roots_differ() {
        let node = NodeKeypair::generate().node_id();
        let ctx = make_ctx(node, Some(cid(0x01)));
        assert!(matches!(
            check_cas(&ctx, Some(cid(0x02))),
            Err(SqlError::CasConflict { .. })
        ));
    }

    #[test]
    fn cas_passes_when_expected_none() {
        let node = NodeKeypair::generate().node_id();
        let ctx = make_ctx(node, None);
        // First write: no expected root supplied.
        assert!(check_cas(&ctx, None).is_ok());
        assert!(check_cas(&ctx, Some(cid(0xFF))).is_ok());
    }

    #[test]
    fn ownership_check_passes_for_owner() {
        let owner = NodeKeypair::generate().node_id();
        let ctx = make_ctx(owner, None);
        assert!(check_ownership(&ctx, &owner).is_ok());
    }

    #[test]
    fn ownership_check_fails_for_non_owner() {
        let owner = NodeKeypair::generate().node_id();
        let non_owner = NodeKeypair::generate().node_id();
        let ctx = make_ctx(non_owner, None);
        assert!(matches!(
            check_ownership(&ctx, &owner),
            Err(SqlError::UnauthorizedWriter { .. })
        ));
    }
}
