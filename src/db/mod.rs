pub mod connections;
pub mod environments;
pub mod keychain;
pub mod schema_cache;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

const MIGRATION_SQL: &str = include_str!("../../migrations/001_initial.sql");
const MIGRATION_002_SQL: &str = include_str!("../../migrations/002_env_is_secret.sql");

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

    Ok(pool)
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
