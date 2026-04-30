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

    /// Test-only seeding of cache entries — bypasses `create_pool` so
    /// cache methods (`invalidate`, `cleanup_expired`, `get_query_timeout`)
    /// can be unit-tested without standing up a live PG/MySQL pool.
    #[cfg(test)]
    pub(crate) async fn insert_for_test(
        &self,
        connection_id: &str,
        name: &str,
        pool: Arc<DatabasePool>,
        last_used: Instant,
        ttl_seconds: u64,
        query_timeout_ms: u64,
    ) {
        let mut pools = self.pools.write().await;
        pools.insert(
            connection_id.to_string(),
            PoolEntry {
                pool,
                name: name.to_string(),
                last_used,
                ttl_seconds,
                query_timeout_ms,
            },
        );
    }

    #[cfg(test)]
    pub(crate) async fn cache_size(&self) -> usize {
        self.pools.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// Minimal `ConnectionLookup` impl that always returns Ok(None) —
    /// stand-in for the cache-only tests where lookup isn't exercised.
    struct NoopLookup;

    #[async_trait::async_trait]
    impl ConnectionLookup for NoopLookup {
        async fn lookup(
            &self,
            _key: &str,
        ) -> Result<Option<crate::db::connections::Connection>, String> {
            Ok(None)
        }
    }

    /// Capturing emitter — counts the calls so `invalidate` /
    /// `cleanup_expired` can assert "we did emit once".
    struct CountingEmitter {
        calls: AtomicUsize,
    }

    impl StatusEmitter for CountingEmitter {
        fn emit_connection_status(&self, _: &str, _: &str, _: &str) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn memory_app_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:").await.unwrap()
    }

    async fn memory_target_pool() -> Arc<DatabasePool> {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        Arc::new(DatabasePool::Sqlite(p))
    }

    #[tokio::test]
    async fn new_standalone_starts_with_empty_cache_and_no_emitter() {
        let app = memory_app_pool().await;
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        assert_eq!(mgr.cache_size().await, 0);
        assert!(mgr.emitter.is_none());
    }

    #[tokio::test]
    async fn new_with_emitter_holds_emitter() {
        let app = memory_app_pool().await;
        let emitter = Arc::new(CountingEmitter {
            calls: AtomicUsize::new(0),
        });
        let mgr = PoolManager::new_with_emitter(Arc::new(NoopLookup), app, emitter);
        assert!(mgr.emitter.is_some());
    }

    #[tokio::test]
    async fn app_pool_returns_borrow_of_inner_pool() {
        let app = memory_app_pool().await;
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        // Use it to run a query — proves we got a working pool back.
        let row: (i32,) = sqlx::query_as("SELECT 1")
            .fetch_one(mgr.app_pool())
            .await
            .unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn invalidate_removes_entry_and_emits_disconnected() {
        let app = memory_app_pool().await;
        let emitter = Arc::new(CountingEmitter {
            calls: AtomicUsize::new(0),
        });
        let mgr = PoolManager::new_with_emitter(Arc::new(NoopLookup), app, emitter.clone());

        let pool = memory_target_pool().await;
        mgr.insert_for_test("c1", "test-conn", pool, Instant::now(), 60, 30_000)
            .await;
        assert_eq!(mgr.cache_size().await, 1);

        mgr.invalidate("c1").await;
        assert_eq!(mgr.cache_size().await, 0);
        assert_eq!(emitter.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidate_unknown_id_is_a_noop_no_emit() {
        let app = memory_app_pool().await;
        let emitter = Arc::new(CountingEmitter {
            calls: AtomicUsize::new(0),
        });
        let mgr = PoolManager::new_with_emitter(Arc::new(NoopLookup), app, emitter.clone());
        mgr.invalidate("nope").await;
        assert_eq!(emitter.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invalidate_without_emitter_drops_silently() {
        let app = memory_app_pool().await;
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        let pool = memory_target_pool().await;
        mgr.insert_for_test("c1", "x", pool, Instant::now(), 60, 30_000)
            .await;
        mgr.invalidate("c1").await;
        assert_eq!(mgr.cache_size().await, 0);
    }

    #[tokio::test]
    async fn cleanup_expired_drops_entries_past_ttl_and_emits_each() {
        let app = memory_app_pool().await;
        let emitter = Arc::new(CountingEmitter {
            calls: AtomicUsize::new(0),
        });
        let mgr = PoolManager::new_with_emitter(Arc::new(NoopLookup), app, emitter.clone());

        let stale = Instant::now() - Duration::from_secs(120);
        let fresh = Instant::now();
        let p1 = memory_target_pool().await;
        let p2 = memory_target_pool().await;
        let p3 = memory_target_pool().await;

        mgr.insert_for_test("expired-1", "a", p1, stale, 60, 30_000).await;
        mgr.insert_for_test("expired-2", "b", p2, stale, 60, 30_000).await;
        mgr.insert_for_test("fresh", "c", p3, fresh, 60, 30_000).await;
        assert_eq!(mgr.cache_size().await, 3);

        mgr.cleanup_expired().await;

        assert_eq!(mgr.cache_size().await, 1);
        assert_eq!(emitter.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cleanup_expired_with_only_fresh_entries_is_a_noop() {
        let app = memory_app_pool().await;
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        let pool = memory_target_pool().await;
        mgr.insert_for_test("fresh", "x", pool, Instant::now(), 300, 30_000)
            .await;
        mgr.cleanup_expired().await;
        assert_eq!(mgr.cache_size().await, 1);
    }

    #[tokio::test]
    async fn get_query_timeout_returns_cached_value_or_none() {
        let app = memory_app_pool().await;
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        let pool = memory_target_pool().await;
        mgr.insert_for_test("c1", "x", pool, Instant::now(), 60, 12_345)
            .await;
        assert_eq!(mgr.get_query_timeout("c1").await, Some(12_345));
        assert_eq!(mgr.get_query_timeout("missing").await, None);
    }

    #[tokio::test]
    async fn get_pool_returns_not_found_when_lookup_returns_none() {
        let app = memory_app_pool().await;
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        match mgr.get_pool("missing").await {
            Ok(_) => panic!("expected not-found error"),
            Err(e) => assert!(e.contains("not found"), "got: {e}"),
        }
    }

    #[tokio::test]
    async fn get_pool_serves_cached_entry_without_calling_lookup() {
        let app = memory_app_pool().await;
        // NoopLookup would error if called; serving from cache must
        // bypass it entirely.
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app);
        let pool = memory_target_pool().await;
        mgr.insert_for_test("c1", "x", pool.clone(), Instant::now(), 60, 30_000)
            .await;
        let returned = mgr.get_pool("c1").await.unwrap();
        // Same Arc as inserted
        assert!(Arc::ptr_eq(&returned, &pool));
    }

    #[tokio::test]
    async fn cleanup_query_log_drops_old_rows_and_caps_recent() {
        let app = memory_app_pool().await;
        sqlx::query(
            r#"CREATE TABLE query_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )"#,
        )
        .execute(&app)
        .await
        .unwrap();
        // 5 old (>30 days) + 5 recent rows
        for _ in 0..5 {
            sqlx::query("INSERT INTO query_log (created_at) VALUES (datetime('now', '-60 days'))")
                .execute(&app)
                .await
                .unwrap();
        }
        for _ in 0..5 {
            sqlx::query("INSERT INTO query_log (created_at) VALUES (datetime('now'))")
                .execute(&app)
                .await
                .unwrap();
        }
        let mgr = PoolManager::new_standalone(Arc::new(NoopLookup), app.clone());
        mgr.cleanup_query_log().await;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM query_log")
            .fetch_one(&app)
            .await
            .unwrap();
        assert_eq!(count, 5, "old rows should be deleted, recent ones kept");
    }
}
