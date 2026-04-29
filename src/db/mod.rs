pub mod chat;
pub mod connections;
pub mod driver;
pub mod environments;
pub mod keychain;
pub mod lookup;
pub mod pool;
pub mod pool_exec_mysql;
pub mod pool_exec_pg;
pub mod pool_exec_sqlite;
pub mod pool_manager;
pub mod query_error;
pub mod schema_cache;
pub mod sql_scanner;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Column, Row, TypeInfo};
use std::path::Path;
use std::str::FromStr;

use connections::{
    contains_multiple_statements, sanitize_query_error, sqlite_row_to_json, ColumnInfo, JsonRow,
};

const MIGRATION_SQL: &str = include_str!("../../migrations/001_initial.sql");
const MIGRATION_002_SQL: &str = include_str!("../../migrations/002_env_is_secret.sql");
const MIGRATION_003_SQL: &str = include_str!("../../migrations/003_chat.sql");
const MIGRATION_004_SQL: &str = include_str!("../../migrations/004_permissions.sql");
const MIGRATION_005_SQL: &str = include_str!("../../migrations/005_audit_log.sql");
const MIGRATION_006_SQL: &str = include_str!("../../migrations/006_schema_cache_schema_name.sql");
const MIGRATION_007_SQL: &str = include_str!("../../migrations/007_connection_readonly.sql");
const MIGRATION_008_SQL: &str = include_str!("../../migrations/008_sqlite_port_null.sql");
const MIGRATION_009_SQL: &str = include_str!("../../migrations/009_block_run_history.sql");
const MIGRATION_010_SQL: &str = include_str!("../../migrations/010_block_settings.sql");
const MIGRATION_011_SQL: &str = include_str!("../../migrations/011_block_examples.sql");

pub async fn init_db(app_data_dir: &Path) -> Result<SqlitePool, sqlx::Error> {
    std::fs::create_dir_all(app_data_dir).ok();

    let db_path = app_data_dir.join("notes.db");
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

    let options = SqliteConnectOptions::from_str(&db_url)?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    run_migrations(&pool).await?;

    // T33: Restrict file permissions on Unix (owner-only read/write)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if db_path.exists() {
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    Ok(pool)
}

// --- Internal DB query (read-only, for audit/settings UI) ---

const MAX_INTERNAL_FETCH_SIZE: u32 = 500;
const MAX_INTERNAL_OFFSET: u32 = 100_000;

#[derive(Debug, Serialize)]
pub struct InternalQueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub has_more: bool,
}

/// Execute a read-only query against the app's internal SQLite database.
/// Only SELECT, WITH, read-only PRAGMA, and EXPLAIN are allowed.
pub async fn query_internal_db(
    pool: &SqlitePool,
    sql: &str,
    offset: u32,
    fetch_size: u32,
) -> Result<InternalQueryResult, String> {
    let trimmed = sql.trim_start();
    let upper = trimmed.to_uppercase();

    // Enforce read-only
    let allowed = upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("EXPLAIN")
        || (upper.starts_with("PRAGMA") && !upper.contains('='));

    if !allowed {
        return Err("Only SELECT queries are allowed on the internal database".to_string());
    }

    if contains_multiple_statements(trimmed) {
        return Err("Multi-statement queries are not allowed".to_string());
    }

    let fetch_size = fetch_size.clamp(1, MAX_INTERNAL_FETCH_SIZE);
    let offset = offset.min(MAX_INTERNAL_OFFSET);

    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    let paginated_sql = format!("SELECT * FROM ({trimmed}) LIMIT {limit} OFFSET {off}");

    let mut rows = sqlx::query(&paginated_sql)
        .fetch_all(pool)
        .await
        .map_err(sanitize_query_error)?;

    let has_more = rows.len() > fetch_size as usize;
    if has_more {
        rows.pop();
    }

    let columns: Vec<ColumnInfo> = if let Some(first) = rows.first() {
        first
            .columns()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_name: c.type_info().name().to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let json_rows: Vec<JsonRow> = rows.iter().map(sqlite_row_to_json).collect();

    Ok(InternalQueryResult {
        columns,
        rows: json_rows,
        has_more,
    })
}

async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Split migration file by statements and execute each
    for statement in MIGRATION_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(pool).await?;
        }
    }

    // Run incremental migrations (idempotent ALTER TABLE)
    for statement in MIGRATION_002_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            // ALTER TABLE may fail if column already exists — that's ok
            let _ = sqlx::query(trimmed).execute(pool).await;
        }
    }

    // Chat tables (CREATE IF NOT EXISTS — idempotent)
    for statement in MIGRATION_003_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(pool).await?;
        }
    }

    // Permission rules + messages.cache_read_tokens (idempotent: CREATE IF NOT EXISTS + ALTER may fail)
    for statement in MIGRATION_004_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            let _ = sqlx::query(trimmed).execute(pool).await;
        }
    }

    // T30: Query audit log (CREATE IF NOT EXISTS — idempotent)
    for statement in MIGRATION_005_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            let _ = sqlx::query(trimmed).execute(pool).await;
        }
    }

    // Stage 7: schema_cache.schema_name (ALTER may fail if column exists — ok)
    for statement in MIGRATION_006_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            let _ = sqlx::query(trimmed).execute(pool).await;
        }
    }

    // Stage 8: connections.is_readonly (ALTER may fail if column exists — ok)
    for statement in MIGRATION_007_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            let _ = sqlx::query(trimmed).execute(pool).await;
        }
    }

    // Stage 8: heal SQLite connections that had port coerced to 0 (idempotent)
    for statement in MIGRATION_008_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            let _ = sqlx::query(trimmed).execute(pool).await;
        }
    }

    // Story 24.6: block run history (CREATE IF NOT EXISTS — idempotent)
    for statement in MIGRATION_009_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(pool).await?;
        }
    }

    // Onda 1: per-block settings (CREATE IF NOT EXISTS — idempotent)
    for statement in MIGRATION_010_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(pool).await?;
        }
    }

    // Onda 3: per-block pinned response examples (CREATE IF NOT EXISTS)
    for statement in MIGRATION_011_SQL.split(';') {
        let trimmed = statement.trim();
        if !trimmed.is_empty() {
            sqlx::query(trimmed).execute(pool).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_init_db_creates_file_and_runs_migrations() {
        let tmp = TempDir::new().unwrap();
        let pool = init_db(tmp.path()).await.unwrap();

        // Verify tables exist by querying them
        let result = sqlx::query("SELECT COUNT(*) as count FROM app_config")
            .fetch_one(&pool)
            .await;
        assert!(result.is_ok());

        let result = sqlx::query("SELECT COUNT(*) as count FROM connections")
            .fetch_one(&pool)
            .await;
        assert!(result.is_ok());

        let result = sqlx::query("SELECT COUNT(*) as count FROM environments")
            .fetch_one(&pool)
            .await;
        assert!(result.is_ok());

        let result = sqlx::query("SELECT COUNT(*) as count FROM block_results")
            .fetch_one(&pool)
            .await;
        assert!(result.is_ok());

        let result = sqlx::query("SELECT COUNT(*) as count FROM schema_cache")
            .fetch_one(&pool)
            .await;
        assert!(result.is_ok());

        pool.close().await;
    }

    #[tokio::test]
    async fn test_init_db_is_idempotent() {
        let tmp = TempDir::new().unwrap();

        // Run twice — should not fail
        let pool1 = init_db(tmp.path()).await.unwrap();
        pool1.close().await;

        let pool2 = init_db(tmp.path()).await.unwrap();
        pool2.close().await;
    }
}
