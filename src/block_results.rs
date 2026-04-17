use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

#[derive(Debug, Serialize)]
pub struct CachedBlockResult {
    pub status: String,
    pub response: String,
    pub total_rows: Option<i64>,
    pub elapsed_ms: i64,
    pub executed_at: String,
}

pub async fn get_block_result(
    pool: &SqlitePool,
    file_path: &str,
    block_hash: &str,
) -> Result<Option<CachedBlockResult>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT status, response, total_rows, elapsed_ms, executed_at
         FROM block_results WHERE file_path = ?1 AND block_hash = ?2",
    )
    .bind(file_path)
    .bind(block_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| CachedBlockResult {
        status: r.get("status"),
        response: r.get("response"),
        total_rows: r.get("total_rows"),
        elapsed_ms: r.get("elapsed_ms"),
        executed_at: r.get("executed_at"),
    }))
}

pub async fn save_block_result(
    pool: &SqlitePool,
    file_path: &str,
    block_hash: &str,
    status: &str,
    response: &str,
    elapsed_ms: i64,
    total_rows: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO block_results (file_path, block_hash, status, response, elapsed_ms, total_rows)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(file_path, block_hash) DO UPDATE SET
           status = excluded.status,
           response = excluded.response,
           elapsed_ms = excluded.elapsed_ms,
           total_rows = excluded.total_rows,
           executed_at = datetime('now')",
    )
    .bind(file_path)
    .bind(block_hash)
    .bind(status)
    .bind(response)
    .bind(elapsed_ms)
    .bind(total_rows)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn delete_block_results_for_file(
    pool: &SqlitePool,
    file_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM block_results WHERE file_path = ?1")
        .bind(file_path)
        .execute(pool)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;
    use tempfile::TempDir;

    async fn setup() -> (SqlitePool, TempDir) {
        let tmp = TempDir::new().unwrap();
        let pool = init_db(tmp.path()).await.unwrap();
        (pool, tmp)
    }

    #[tokio::test]
    async fn test_get_returns_none_when_empty() {
        let (pool, _tmp) = setup().await;
        let result = get_block_result(&pool, "test.md", "abc123").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_save_and_get() {
        let (pool, _tmp) = setup().await;

        save_block_result(&pool, "test.md", "hash1", "success", r#"{"ok":true}"#, 150, None)
            .await
            .unwrap();

        let result = get_block_result(&pool, "test.md", "hash1").await.unwrap();
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.status, "success");
        assert_eq!(r.response, r#"{"ok":true}"#);
        assert_eq!(r.elapsed_ms, 150);
        assert!(r.total_rows.is_none());
    }

    #[tokio::test]
    async fn test_save_upserts() {
        let (pool, _tmp) = setup().await;

        save_block_result(&pool, "test.md", "hash1", "success", r#"{"v":1}"#, 100, None)
            .await
            .unwrap();
        save_block_result(&pool, "test.md", "hash1", "success", r#"{"v":2}"#, 200, Some(5))
            .await
            .unwrap();

        let r = get_block_result(&pool, "test.md", "hash1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.response, r#"{"v":2}"#);
        assert_eq!(r.elapsed_ms, 200);
        assert_eq!(r.total_rows, Some(5));
    }

    #[tokio::test]
    async fn test_different_hash_different_result() {
        let (pool, _tmp) = setup().await;

        save_block_result(&pool, "test.md", "hash1", "success", "r1", 100, None)
            .await
            .unwrap();
        save_block_result(&pool, "test.md", "hash2", "error", "r2", 50, None)
            .await
            .unwrap();

        let r1 = get_block_result(&pool, "test.md", "hash1").await.unwrap().unwrap();
        let r2 = get_block_result(&pool, "test.md", "hash2").await.unwrap().unwrap();
        assert_eq!(r1.status, "success");
        assert_eq!(r2.status, "error");
    }

    #[tokio::test]
    async fn test_delete_for_file() {
        let (pool, _tmp) = setup().await;

        save_block_result(&pool, "test.md", "h1", "success", "r1", 100, None)
            .await
            .unwrap();
        save_block_result(&pool, "test.md", "h2", "success", "r2", 100, None)
            .await
            .unwrap();
        save_block_result(&pool, "other.md", "h1", "success", "r3", 100, None)
            .await
            .unwrap();

        delete_block_results_for_file(&pool, "test.md").await.unwrap();

        assert!(get_block_result(&pool, "test.md", "h1").await.unwrap().is_none());
        assert!(get_block_result(&pool, "test.md", "h2").await.unwrap().is_none());
        assert!(get_block_result(&pool, "other.md", "h1").await.unwrap().is_some());
    }
}
