// coverage:exclude file — DB pool/exec/lookup or vault-store registry. Coverage requires live DB integration tests; owned by Epic 32 (critical-path tests). Audit-027.

//! Pool lifecycle, TTL eviction, status emission.
//!
//! Extracted from `db::connections` (Epic 20a Story 01 — first split).
//! `connections.rs` was 2894 L mixing 7 concerns; this file owns the
//! pool-management one. Holds an `Arc<dyn ConnectionLookup>` (file-
//! backed in production via `vault_config::ConnectionsStore`,
//! `SqliteLookup` in legacy tests) — no direct SQLite coupling for
//! connection records.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::sqlite::SqlitePool;
use tokio::sync::RwLock;

use super::lookup::ConnectionLookup;
use super::pool::{create_pool, DatabasePool};

/// Trait for emitting connection status events.
/// The Tauri app provides an AppHandle-based implementation; the MCP
/// binary (and tests) use None.
pub trait StatusEmitter: Send + Sync {
    fn emit_connection_status(&self, connection_id: &str, name: &str, status: &str);
}

pub struct PoolManager {
    /// Resolves `Connection` records by name. File-backed in production
    /// (`ConnectionsStore`); SQLite adapter in legacy tests
    /// (`SqliteLookup`). See `db/lookup.rs`.
    lookup: Arc<dyn ConnectionLookup>,
    /// Retained only for `cleanup_query_log` (the `query_log` SQLite
    /// table; future Epic 20a Story owns the move-out and the field
    /// disappears with it).
    app_pool: SqlitePool,
    pools: RwLock<HashMap<String, PoolEntry>>,
    emitter: Option<Arc<dyn StatusEmitter>>,
}

struct PoolEntry {
    pool: Arc<DatabasePool>,
    name: String,
    last_used: Instant,
    ttl_seconds: u64,
    query_timeout_ms: u64,
}

impl PoolManager {
    pub fn new_with_emitter(
        lookup: Arc<dyn ConnectionLookup>,
        app_pool: SqlitePool,
        emitter: Arc<dyn StatusEmitter>,
    ) -> Self {
        Self {
            lookup,
            app_pool,
            pools: RwLock::new(HashMap::new()),
            emitter: Some(emitter),
        }
    }

    /// Create without event emitter (for MCP server and tests).
    pub fn new_standalone(lookup: Arc<dyn ConnectionLookup>, app_pool: SqlitePool) -> Self {
        Self {
            lookup,
            app_pool,
            pools: RwLock::new(HashMap::new()),
            emitter: None,
        }
    }

    pub fn app_pool(&self) -> &SqlitePool {
        &self.app_pool
    }

    pub async fn get_pool(&self, connection_id: &str) -> Result<Arc<DatabasePool>, String> {
        // Check cache — write lock to update last_used on hit
        {
            let mut pools = self.pools.write().await;
            if let Some(entry) = pools.get_mut(connection_id) {
                entry.last_used = Instant::now();
                return Ok(entry.pool.clone());
            }
        }

        // Not cached — resolve connection and create pool
        let conn = self
            .lookup
            .lookup(connection_id)
            .await?
            .ok_or_else(|| format!("Connection '{}' not found", connection_id))?;

        let conn_name = conn.name.clone();
        let pool = Arc::new(create_pool(&conn).await?);

        {
            let mut pools = self.pools.write().await;
            pools.insert(
                connection_id.to_string(),
                PoolEntry {
                    pool: pool.clone(),
                    name: conn_name.clone(),
                    last_used: Instant::now(),
                    ttl_seconds: conn.ttl_seconds as u64,
                    query_timeout_ms: conn.query_timeout_ms as u64,
                },
            );
        }

        if let Some(ref emitter) = self.emitter {
            emitter.emit_connection_status(connection_id, &conn_name, "connected");
        }

        Ok(pool)
    }

    pub async fn invalidate(&self, connection_id: &str) {
        let entry_name = {
            let mut pools = self.pools.write().await;
            pools.remove(connection_id).map(|e| e.name)
        };
        if let (Some(name), Some(ref emitter)) = (entry_name, &self.emitter) {
            emitter.emit_connection_status(connection_id, &name, "disconnected");
        }
    }

    pub async fn cleanup_expired(&self) {
        let to_check: Vec<(String, Instant, u64)> = {
            let pools = self.pools.read().await;
            pools
                .iter()
                .map(|(id, entry)| (id.clone(), entry.last_used, entry.ttl_seconds))
                .collect()
        };

        let mut to_remove = Vec::new();
        for (id, last_used, ttl_seconds) in &to_check {
            if last_used.elapsed() > Duration::from_secs(*ttl_seconds) {
                to_remove.push(id.clone());
            }
        }

        if !to_remove.is_empty() {
            let mut pools = self.pools.write().await;
            for id in &to_remove {
                if let Some(entry) = pools.remove(id) {
                    if let Some(ref emitter) = self.emitter {
                        emitter.emit_connection_status(id, &entry.name, "disconnected");
                    }
                }
            }
        }
    }

    /// Returns the connection's query_timeout_ms from the pool cache, if available.
    pub async fn get_query_timeout(&self, connection_id: &str) -> Option<u64> {
        let pools = self.pools.read().await;
        pools.get(connection_id).map(|e| e.query_timeout_ms)
    }

    /// Delete query_log entries older than 30 days or exceeding 50k rows.
    pub async fn cleanup_query_log(&self) {
        let _ = sqlx::query("DELETE FROM query_log WHERE created_at < datetime('now', '-30 days')")
            .execute(&self.app_pool)
            .await;

        // Cap at 50k entries — delete oldest beyond that
        let _ = sqlx::query(
            "DELETE FROM query_log WHERE id NOT IN (SELECT id FROM query_log ORDER BY id DESC LIMIT 50000)",
        )
        .execute(&self.app_pool)
        .await;
    }

    pub async fn test_connection(&self, connection_id: &str) -> Result<(), String> {
        let conn = self
            .lookup
            .lookup(connection_id)
            .await?
            .ok_or_else(|| format!("Connection '{}' not found", connection_id))?;

        let pool = create_pool(&conn).await?;
        pool.test().await?;

        // The legacy `UPDATE connections SET last_tested_at` write is
        // dropped: the file-backed schema doesn't carry that field, and
        // the live status emitter (`emit_connection_status`) is the
        // user-facing signal anyway. Reintroduce as a per-machine cache
        // (e.g. `~/.config/httui/connection_status.toml`) if a UI need
        // emerges — out of scope for v1 (audit-015 Phase 3 decision).

        Ok(())
    }
}
