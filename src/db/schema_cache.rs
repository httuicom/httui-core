// coverage:exclude file — DB pool/exec/lookup or vault-store registry. Coverage requires live DB integration tests; owned by Epic 32 (critical-path tests). Audit-027.

use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use super::connections::{DatabasePool, PoolManager};

#[derive(Debug, Clone, Serialize)]
pub struct SchemaEntry {
    /// Schema / database namespace. `None` for SQLite (single-namespace).
    /// For MySQL this is the active database name; for Postgres the
    /// `table_schema` column value, e.g. `public`, `vendas`, `app`.
    pub schema_name: Option<String>,
    pub table_name: String,
    pub column_name: String,
    pub data_type: Option<String>,
}

/// Introspect schema from the target database and cache results.
pub async fn introspect_schema(
    conn_manager: &PoolManager,
    app_pool: &SqlitePool,
    connection_id: &str,
) -> Result<Vec<SchemaEntry>, String> {
    let pool = conn_manager.get_pool(connection_id).await?;

    let entries = match pool.as_ref() {
        DatabasePool::Sqlite(p) => introspect_sqlite(p).await?,
        DatabasePool::Postgres(p) => introspect_postgres(p).await?,
        DatabasePool::MySql(p) => introspect_mysql(p).await?,
    };

    // Save to cache
    save_to_cache(app_pool, connection_id, &entries).await?;

    Ok(entries)
}

/// Get cached schema entries. Returns None if cache is expired or empty.
pub async fn get_cached_schema(
    app_pool: &SqlitePool,
    connection_id: &str,
    ttl_seconds: i64,
) -> Result<Option<Vec<SchemaEntry>>, String> {
    let rows = sqlx::query(
        r#"SELECT schema_name, table_name, column_name, data_type, cached_at
        FROM schema_cache
        WHERE connection_id = ?
        AND (julianday('now') - julianday(cached_at)) * 86400 < ?
        ORDER BY schema_name IS NULL, schema_name, table_name, column_name"#,
    )
    .bind(connection_id)
    .bind(ttl_seconds)
    .fetch_all(app_pool)
    .await
    .map_err(|e| e.to_string())?;

    if rows.is_empty() {
        return Ok(None);
    }

    let entries: Vec<SchemaEntry> = rows
        .iter()
        .map(|row| SchemaEntry {
            schema_name: row.try_get("schema_name").ok(),
            table_name: row.get("table_name"),
            column_name: row.get("column_name"),
            data_type: row.get("data_type"),
        })
        .collect();

    Ok(Some(entries))
}

async fn save_to_cache(
    app_pool: &SqlitePool,
    connection_id: &str,
    entries: &[SchemaEntry],
) -> Result<(), String> {
    // Clear existing cache for this connection
    sqlx::query("DELETE FROM schema_cache WHERE connection_id = ?")
        .bind(connection_id)
        .execute(app_pool)
        .await
        .map_err(|e| e.to_string())?;

    // Insert new entries
    for entry in entries {
        sqlx::query(
            r#"INSERT INTO schema_cache (connection_id, schema_name, table_name, column_name, data_type)
            VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(connection_id)
        .bind(&entry.schema_name)
        .bind(&entry.table_name)
        .bind(&entry.column_name)
        .bind(&entry.data_type)
        .execute(app_pool)
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

// --- Driver-specific introspection ---

async fn introspect_sqlite(pool: &sqlx::SqlitePool) -> Result<Vec<SchemaEntry>, String> {
    // Include tables AND views. Exclude sqlite-internal objects.
    let objects = sqlx::query(
        "SELECT name FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut entries = Vec::new();

    for row in &objects {
        let name: String = row.get("name");
        let pragma = format!("PRAGMA table_info(\"{}\")", name);
        let columns = sqlx::query(&pragma)
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;

        for col in &columns {
            entries.push(SchemaEntry {
                schema_name: None,
                table_name: name.clone(),
                column_name: col.get("name"),
                data_type: col.try_get("type").ok(),
            });
        }
    }

    Ok(entries)
}

async fn introspect_postgres(pool: &sqlx::PgPool) -> Result<Vec<SchemaEntry>, String> {
    // `information_schema.columns` already spans tables, views, and materialized
    // views. Exclude catalog schemas so regular users don't drown in 2k+ entries.
    let rows = sqlx::query(
        r#"SELECT table_schema, table_name, column_name, data_type
        FROM information_schema.columns
        WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
          AND table_schema NOT LIKE 'pg_%'
        ORDER BY table_schema, table_name, ordinal_position"#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows
        .iter()
        .map(|row| SchemaEntry {
            schema_name: row.try_get::<String, _>("table_schema").ok(),
            table_name: row.get("table_name"),
            column_name: row.get("column_name"),
            data_type: row.try_get("data_type").ok(),
        })
        .collect())
}

/// Decode a MySQL column as UTF-8 String, tolerating VARBINARY/BLOB columns.
/// Some MySQL proxies (notably ProxySQL) return information_schema text columns
/// as VARBINARY, which fails String decoding. Fall back to raw bytes.
fn mysql_str(row: &sqlx::mysql::MySqlRow, col: &str) -> Option<String> {
    if let Ok(s) = row.try_get::<String, _>(col) {
        return Some(s);
    }
    if let Ok(b) = row.try_get::<Vec<u8>, _>(col) {
        return Some(String::from_utf8_lossy(&b).into_owned());
    }
    None
}

async fn introspect_mysql(pool: &sqlx::MySqlPool) -> Result<Vec<SchemaEntry>, String> {
    // DATABASE() resolves via the USE issued in after_connect (see
    // connections.rs build). information_schema.columns already includes
    // columns for views; no extra join needed.
    let rows = sqlx::query(
        r#"SELECT table_schema, table_name, column_name, data_type
        FROM information_schema.columns
        WHERE table_schema = DATABASE()
        ORDER BY table_name, ordinal_position"#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows
        .iter()
        .filter_map(|row| {
            let table_name = mysql_str(row, "TABLE_NAME")?;
            let column_name = mysql_str(row, "COLUMN_NAME")?;
            let data_type = mysql_str(row, "DATA_TYPE");
            let schema_name = mysql_str(row, "TABLE_SCHEMA");
            Some(SchemaEntry {
                schema_name,
                table_name,
                column_name,
                data_type,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connections::{CreateConnection, PoolManager};
    use std::sync::Arc;

    async fn setup_test_env() -> (Arc<PoolManager>, SqlitePool, String) {
        let app_pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        // Create app tables
        sqlx::query(
            r#"CREATE TABLE connections (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                driver TEXT NOT NULL CHECK (driver IN ('postgres', 'mysql', 'sqlite')),
                host TEXT, port INTEGER, database_name TEXT,
                username TEXT, password TEXT,
                ssl_mode TEXT DEFAULT 'disable',
                timeout_ms INTEGER DEFAULT 10000,
                query_timeout_ms INTEGER DEFAULT 30000,
                ttl_seconds INTEGER DEFAULT 300,
                max_pool_size INTEGER DEFAULT 5,
                is_readonly INTEGER NOT NULL DEFAULT 0,
                last_tested_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )"#,
        )
        .execute(&app_pool)
        .await
        .unwrap();

        sqlx::query(
            r#"CREATE TABLE schema_cache (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                connection_id TEXT NOT NULL,
                schema_name TEXT,
                table_name TEXT NOT NULL,
                column_name TEXT NOT NULL,
                data_type TEXT,
                cached_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(connection_id, schema_name, table_name, column_name)
            )"#,
        )
        .execute(&app_pool)
        .await
        .unwrap();

        let conn = crate::db::connections::create_connection(
            &app_pool,
            CreateConnection {
                name: "test-sqlite".to_string(),
                driver: "sqlite".to_string(),
                host: None,
                port: None,
                database_name: Some(":memory:".to_string()),
                username: None,
                password: None,
                ssl_mode: None,
                timeout_ms: None,
                query_timeout_ms: None,
                ttl_seconds: None,
                max_pool_size: None,
                is_readonly: None,
            },
        )
        .await
        .unwrap();

        let manager = Arc::new(PoolManager::new_standalone(
            crate::db::lookup::SqliteLookup::new(app_pool.clone()),
            app_pool.clone(),
        ));
        (manager, app_pool, conn.id)
    }

    #[tokio::test]
    async fn test_introspect_sqlite_schema() {
        let (manager, app_pool, conn_id) = setup_test_env().await;

        // Create tables in target database
        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)")
                    .execute(p)
                    .await
                    .unwrap();
                sqlx::query(
                    "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
                )
                .execute(p)
                .await
                .unwrap();
            }
            _ => panic!("Expected SQLite"),
        }

        let entries = introspect_schema(&manager, &app_pool, &conn_id)
            .await
            .unwrap();

        assert!(entries.len() >= 6); // 3 columns per table
        assert!(entries
            .iter()
            .any(|e| e.table_name == "users" && e.column_name == "name"));
        assert!(entries
            .iter()
            .any(|e| e.table_name == "posts" && e.column_name == "title"));
    }

    #[tokio::test]
    async fn test_cached_schema() {
        let (manager, app_pool, conn_id) = setup_test_env().await;

        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE items (id INTEGER, val TEXT)")
                    .execute(p)
                    .await
                    .unwrap();
            }
            _ => panic!("Expected SQLite"),
        }

        // Should be empty before introspection
        let cached = get_cached_schema(&app_pool, &conn_id, 300).await.unwrap();
        assert!(cached.is_none());

        // Introspect (fills cache)
        introspect_schema(&manager, &app_pool, &conn_id)
            .await
            .unwrap();

        // Now should have cached entries
        let cached = get_cached_schema(&app_pool, &conn_id, 300)
            .await
            .unwrap()
            .expect("Should have cached schema");

        assert!(cached
            .iter()
            .any(|e| e.table_name == "items" && e.column_name == "id"));
        assert!(cached
            .iter()
            .any(|e| e.table_name == "items" && e.column_name == "val"));
    }

    #[tokio::test]
    async fn test_introspect_sqlite_includes_views() {
        let (manager, app_pool, conn_id) = setup_test_env().await;
        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER)")
                    .execute(p)
                    .await
                    .unwrap();
                sqlx::query("CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100")
                    .execute(p)
                    .await
                    .unwrap();
            }
            _ => panic!("Expected SQLite"),
        }

        let entries = introspect_schema(&manager, &app_pool, &conn_id)
            .await
            .unwrap();

        assert!(
            entries.iter().any(|e| e.table_name == "orders"),
            "expected orders table"
        );
        assert!(
            entries.iter().any(|e| e.table_name == "big_orders"),
            "expected big_orders view to appear alongside tables"
        );
        assert!(entries.iter().all(|e| e.schema_name.is_none()));
    }
}
