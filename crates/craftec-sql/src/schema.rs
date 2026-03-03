//! Schema management for CraftSQL.
//!
//! CraftSQL databases follow a **single-writer-per-identity** model, which
//! makes schema migrations straightforward: there is no risk of concurrent
//! DDL conflicts because only the owner can issue `ALTER TABLE`, `CREATE
//! TABLE`, or other schema-mutating statements.
//!
//! ## Migration model
//! - Migrations are plain SQL strings (DDL + optional DML).
//! - The owner calls [`migrate`] to apply a migration, which is treated as a
//!   regular write through the CID-VFS commit pipeline.
//! - Each migration produces a new root CID, providing an audit trail of
//!   schema changes.
//! - Rollback is achieved by pinning an older root CID (snapshot rewind).
//!
//! ## No migration table required
//! Because the page-index root CID acts as a cryptographic state fingerprint,
//! migration history is implicit in the CID chain.  A future tooling layer may
//! layer a `_craftec_migrations` table on top for human-readable history.

use craftec_types::NodeId;

use crate::database::CraftDatabase;
use crate::error::{Result, SqlError};

/// Apply `migration_sql` to `db` as a schema migration.
///
/// This is a convenience wrapper around [`CraftDatabase::execute`] that
/// enforces ownership and adds migration-specific tracing/error context.
///
/// # Arguments
/// * `db` — the target [`CraftDatabase`].
/// * `owner` — the Ed25519 identity executing the migration.  Must match the
///   database owner or the call will be rejected.
/// * `migration_sql` — one or more SQL DDL statements separated by `;`.
///
/// # Errors
/// - [`SqlError::UnauthorizedWriter`] — if `owner` is not the database owner.
/// - [`SqlError::MigrationFailed`] — wraps any VFS or SQL-level error.
///
/// # Example
/// ```rust,ignore
/// migrate(&db, &owner_node, "ALTER TABLE users ADD COLUMN email TEXT").await?;
/// ```
pub async fn migrate(db: &CraftDatabase, owner: &NodeId, migration_sql: &str) -> Result<()> {
    validate_migration_sql(migration_sql)?;

    tracing::info!(
        db_id = %db.db_id(),
        owner = %owner,
        sql_len = migration_sql.len(),
        "CraftSQL: applying schema migration",
    );

    db.execute(migration_sql, owner).await.map_err(|e| match e {
        SqlError::UnauthorizedWriter { .. } => e,
        other => SqlError::MigrationFailed(other.to_string()),
    })?;

    tracing::info!(
        db_id = %db.db_id(),
        "CraftSQL: schema migration applied",
    );

    Ok(())
}

/// Validate that `migration_sql` is non-empty and appears to contain at least
/// one DDL keyword.
///
/// This is a lightweight pre-flight check — the VFS layer performs the
/// authoritative validation when the SQL is executed.
///
/// # Errors
/// Returns [`SqlError::MigrationFailed`] if the SQL is empty or contains only
/// whitespace.
pub fn validate_migration_sql(migration_sql: &str) -> Result<()> {
    let trimmed = migration_sql.trim();
    if trimmed.is_empty() {
        return Err(SqlError::MigrationFailed(
            "migration SQL must not be empty".into(),
        ));
    }

    // Warn (but do not reject) if no DDL keywords are present.
    let upper = trimmed.to_uppercase();
    let has_ddl = ["CREATE", "ALTER", "DROP", "RENAME"]
        .iter()
        .any(|kw| upper.contains(kw));

    if !has_ddl {
        tracing::warn!(
            sql = migration_sql,
            "CraftSQL: migration SQL contains no DDL keywords — is this intentional?",
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_migration_sql_rejected() {
        assert!(validate_migration_sql("").is_err());
        assert!(validate_migration_sql("   ").is_err());
    }

    #[test]
    fn non_empty_sql_accepted() {
        assert!(validate_migration_sql("CREATE TABLE t (id INTEGER PRIMARY KEY)").is_ok());
        assert!(validate_migration_sql("ALTER TABLE t ADD COLUMN x TEXT").is_ok());
    }

    #[test]
    fn dml_only_sql_accepted_with_warning() {
        // DML-only migrations are unusual but not rejected.
        assert!(validate_migration_sql("INSERT INTO t VALUES (1)").is_ok());
    }
}
