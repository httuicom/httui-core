// coverage:exclude file — DB pool/exec/lookup or vault-store registry. Coverage requires live DB integration tests; owned by Epic 32 (critical-path tests). Audit-027.

//! Connection lookup abstraction. Decouples `PoolManager` from the
//! concrete storage backend: production reads from
//! `vault_config::ConnectionsStore` (file-backed), tests use a mock,
//! and the legacy `SqlitePool`-backed lookup remains available as
//! an adapter for the SQLite test fixtures that have not yet
//! migrated to file-backed (Epic 20a Story 01 cleanup).
//!
//! Closes the DIP item from `tech-debt.md` —
//! `PoolManager::new(app_pool: SqlitePool, ...)` no longer hardwires
//! SQLite as the source of connection records (Epic 19 Story 02
//! Phase 3; audit-015).

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::sqlite::SqlitePool;

use super::connections::Connection;
use crate::vault_config::ConnectionsStore;

/// Resolve a `Connection` by name (primary key in the file-backed
/// world) or by id (primary key in the legacy SQLite world — adapters
/// accept both forms during the cutover window).
#[async_trait]
pub trait ConnectionLookup: Send + Sync {
    async fn lookup(&self, key: &str) -> Result<Option<Connection>, String>;
}

/// File-backed lookup — the production path. Delegates to
/// `ConnectionsStore::get_legacy`, which already returns the exact
/// `Connection` shape `PoolManager` expects.
#[async_trait]
impl ConnectionLookup for ConnectionsStore {
    async fn lookup(&self, name: &str) -> Result<Option<Connection>, String> {
        self.get_legacy(name).await
    }
}

/// Adapter for tests/fixtures that still seed `connections` rows
/// into the legacy SQLite-backed table. Production code does NOT
/// use this — desktop / tui / mcp construct a `ConnectionsStore`
/// instead.
pub struct SqliteLookup {
    pool: SqlitePool,
}

impl SqliteLookup {
    pub fn new(pool: SqlitePool) -> Arc<Self> {
        Arc::new(Self { pool })
    }
}

#[async_trait]
impl ConnectionLookup for SqliteLookup {
    async fn lookup(&self, key: &str) -> Result<Option<Connection>, String> {
        // Legacy lookup accepted ids; allow either form to bridge the
        // gap during the cutover.
        super::connections::get_connection(&self.pool, key).await
    }
}
