//! Backend-specific operator maintenance ops.
//!
//! Distinct from `ConfigStore` (which abstracts portable config CRUD): these
//! ops — checkpoint, file-level backup, file-level restore — only have
//! meaningful implementations for embedded/file-backed backends like SQLite.
//! A future Postgres backend would either supply its own maintenance handle
//! (e.g. a `pg_basebackup` wrapper) or reject these calls and direct
//! operators at external tooling.
//!
//! See ADR-0002 (component boundaries) and `docs/internal/implementation/
//! adr-0002-review-2026-05-04.md` § P4 sub-fix 1 for the boundary rationale.

use std::path::Path;

use async_trait::async_trait;

#[async_trait]
pub trait OperatorMaintenanceBackend: Send + Sync + 'static {
    async fn checkpoint(&self) -> anyhow::Result<()>;
    async fn backup_to_path(&self, dst: &Path) -> anyhow::Result<()>;
    async fn restore_from_path(&self, src: &Path) -> anyhow::Result<()>;
}
