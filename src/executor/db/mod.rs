use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Instant;

use super::{BlockResult, Executor, ExecutorError};
use crate::db::connections::PoolManager;

#[derive(Debug, Deserialize)]
struct DbParams {
    connection_id: String,
    query: String,
    #[serde(default)]
    bind_values: Vec<serde_json::Value>,
    #[serde(default)]
    offset: u32,
    #[serde(default = "default_fetch_size")]
    fetch_size: u32,
    timeout_ms: Option<u64>,
}

fn default_fetch_size() -> u32 {
    80
}

pub struct DbExecutor {
    conn_manager: Arc<PoolManager>,
}

impl DbExecutor {
    pub fn new(conn_manager: Arc<PoolManager>) -> Self {
        Self { conn_manager }
    }
}

#[async_trait]
impl Executor for DbExecutor {
    fn block_type(&self) -> &str {
        "db"
    }

    async fn validate(&self, params: &serde_json::Value) -> Result<(), String> {
        let p: DbParams =
            serde_json::from_value(params.clone()).map_err(|e| format!("Invalid params: {e}"))?;

        if p.connection_id.trim().is_empty() {
            return Err("connection_id is required".to_string());
        }
        if p.query.trim().is_empty() {
            return Err("query is required".to_string());
        }
        if p.fetch_size == 0 {
            return Err("fetch_size must be >= 1".to_string());
        }

        Ok(())
    }

    async fn execute(&self, params: serde_json::Value) -> Result<BlockResult, ExecutorError> {
        let p: DbParams = serde_json::from_value(params)
            .map_err(|e| ExecutorError(format!("Invalid params: {e}")))?;

        let pool = self
            .conn_manager
            .get_pool(&p.connection_id)
            .await
            .map_err(ExecutorError)?;

        let start = Instant::now();

        // Apply timeout if specified
        let result = if let Some(timeout_ms) = p.timeout_ms {
            let timeout = std::time::Duration::from_millis(timeout_ms);
            tokio::time::timeout(
                timeout,
                pool.execute_query(&p.query, &p.bind_values, p.offset, p.fetch_size),
            )
            .await
            .map_err(|_| ExecutorError(format!("Query timed out after {}ms", timeout_ms)))?
            .map_err(ExecutorError)?
        } else {
            pool.execute_query(&p.query, &p.bind_values, p.offset, p.fetch_size)
                .await
                .map_err(ExecutorError)?
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        if result.is_select {
            let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();

            let columns: Vec<serde_json::Value> = result
                .columns
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "type": c.type_name,
                    })
                })
                .collect();

            // Convert rows from arrays to objects keyed by column name
            let rows: Vec<serde_json::Value> = result
                .rows
                .iter()
                .map(|row| {
                    let obj: serde_json::Map<String, serde_json::Value> = col_names
                        .iter()
                        .zip(row.iter())
                        .map(|(name, val)| (name.to_string(), val.clone()))
                        .collect();
                    serde_json::Value::Object(obj)
                })
                .collect();

            Ok(BlockResult {
                status: "success".to_string(),
                data: serde_json::json!({
                    "columns": columns,
                    "rows": rows,
                    "has_more": result.has_more,
                }),
                duration_ms,
            })
        } else {
            Ok(BlockResult {
                status: "success".to_string(),
                data: serde_json::json!({
                    "rows_affected": result.rows_affected,
                }),
                duration_ms,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connections::{CreateConnection, PoolManager};
    use sqlx::sqlite::SqlitePool;

    async fn setup_test_env() -> (Arc<PoolManager>, String) {
        // Create the app pool with connections table
        let app_pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
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
                last_tested_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )"#,
        )
        .execute(&app_pool)
        .await
        .unwrap();

        // Create a test SQLite connection pointing to in-memory
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
            },
        )
        .await
        .unwrap();

        let manager = Arc::new(PoolManager::new_standalone(app_pool));
        (manager, conn.id)
    }

    #[tokio::test]
    async fn test_db_executor_validate() {
        let (manager, _) = setup_test_env().await;
        let executor = DbExecutor::new(manager);

        // Missing connection_id
        let result = executor
            .validate(&serde_json::json!({
                "connection_id": "",
                "query": "SELECT 1"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("connection_id"));

        // Missing query
        let result = executor
            .validate(&serde_json::json!({
                "connection_id": "some-id",
                "query": ""
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("query"));

        // Valid
        let result = executor
            .validate(&serde_json::json!({
                "connection_id": "some-id",
                "query": "SELECT 1"
            }))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_db_executor_select() {
        let (manager, conn_id) = setup_test_env().await;

        // Create a table in the target database
        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            crate::db::connections::DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE test (id INTEGER, name TEXT)")
                    .execute(p)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO test VALUES (1, 'alice'), (2, 'bob')")
                    .execute(p)
                    .await
                    .unwrap();
            }
            _ => panic!("Expected SQLite pool"),
        }

        let executor = DbExecutor::new(manager);
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "SELECT * FROM test",
            }))
            .await
            .unwrap();

        assert_eq!(result.status, "success");
        assert_eq!(result.data["has_more"], false);
        assert_eq!(result.data["rows"].as_array().unwrap().len(), 2);
        assert_eq!(result.data["columns"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_db_executor_mutation() {
        let (manager, conn_id) = setup_test_env().await;

        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            crate::db::connections::DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE items (id INTEGER, val TEXT)")
                    .execute(p)
                    .await
                    .unwrap();
            }
            _ => panic!("Expected SQLite pool"),
        }

        let executor = DbExecutor::new(manager);
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "INSERT INTO items VALUES (1, 'hello'), (2, 'world')",
            }))
            .await
            .unwrap();

        assert_eq!(result.status, "success");
        assert_eq!(result.data["rows_affected"], 2);
    }

    #[tokio::test]
    async fn test_db_executor_with_bind_params() {
        let (manager, conn_id) = setup_test_env().await;

        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            crate::db::connections::DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE users (id INTEGER, name TEXT, active INTEGER)")
                    .execute(p)
                    .await
                    .unwrap();
                sqlx::query(
                    "INSERT INTO users VALUES (1, 'alice', 1), (2, 'bob', 0), (3, 'charlie', 1)",
                )
                .execute(p)
                .await
                .unwrap();
            }
            _ => panic!("Expected SQLite pool"),
        }

        let executor = DbExecutor::new(manager);
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "SELECT * FROM users WHERE active = ?",
                "bind_values": [1],
            }))
            .await
            .unwrap();

        assert_eq!(result.status, "success");
        assert_eq!(result.data["has_more"], false);
        assert_eq!(result.data["rows"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_db_executor_progressive_fetch() {
        let (manager, conn_id) = setup_test_env().await;

        let pool = manager.get_pool(&conn_id).await.unwrap();
        match pool.as_ref() {
            crate::db::connections::DatabasePool::Sqlite(p) => {
                sqlx::query("CREATE TABLE nums (n INTEGER)")
                    .execute(p)
                    .await
                    .unwrap();
                for i in 1..=15 {
                    sqlx::query("INSERT INTO nums VALUES (?)")
                        .bind(i)
                        .execute(p)
                        .await
                        .unwrap();
                }
            }
            _ => panic!("Expected SQLite pool"),
        }

        let executor = DbExecutor::new(manager);

        // First fetch: offset=0, fetch_size=5 → 5 rows, has_more=true
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "SELECT * FROM nums",
                "offset": 0,
                "fetch_size": 5,
            }))
            .await
            .unwrap();

        assert_eq!(result.data["has_more"], true);
        assert_eq!(result.data["rows"].as_array().unwrap().len(), 5);

        // Second fetch: offset=5, fetch_size=5 → 5 rows, has_more=true
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "SELECT * FROM nums",
                "offset": 5,
                "fetch_size": 5,
            }))
            .await
            .unwrap();

        assert_eq!(result.data["has_more"], true);
        assert_eq!(result.data["rows"].as_array().unwrap().len(), 5);

        // Third fetch: offset=10, fetch_size=5 → 5 rows, has_more=false
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "SELECT * FROM nums",
                "offset": 10,
                "fetch_size": 5,
            }))
            .await
            .unwrap();

        assert_eq!(result.data["has_more"], false);
        assert_eq!(result.data["rows"].as_array().unwrap().len(), 5);

        // Fourth fetch: offset=15, fetch_size=5 → 0 rows, has_more=false
        let result = executor
            .execute(serde_json::json!({
                "connection_id": conn_id,
                "query": "SELECT * FROM nums",
                "offset": 15,
                "fetch_size": 5,
            }))
            .await
            .unwrap();

        assert_eq!(result.data["has_more"], false);
        assert_eq!(result.data["rows"].as_array().unwrap().len(), 0);
    }
}
