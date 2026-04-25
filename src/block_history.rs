//! Block run history — Story 24.6.
//!
//! Stores metadata about HTTP block runs (method, URL canonical, status,
//! sizes, elapsed, timestamp) in SQLite. Body of request/response is NEVER
//! persisted here — privacy-by-default. The drawer reads the last 10 entries
//! per (file_path, alias).
//!
//! Trim policy: after each insert we delete rows for the same
//! (file_path, alias) keeping only the most recent N (default 10). Cap is
//! a private constant — a global retention setting can be wired later.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

const HISTORY_CAP: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_path: String,
    pub block_alias: String,
    pub method: String,
    pub url_canonical: String,
    pub status: Option<i64>,
    pub request_size: Option<i64>,
    pub response_size: Option<i64>,
    pub elapsed_ms: Option<i64>,
    pub outcome: String,
    pub ran_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertEntry {
    pub file_path: String,
    pub block_alias: String,
    pub method: String,
    pub url_canonical: String,
    pub status: Option<i64>,
    pub request_size: Option<i64>,
    pub response_size: Option<i64>,
    pub elapsed_ms: Option<i64>,
    pub outcome: String,
}

/// Insert a new history entry and trim the oldest rows for the same
/// (file_path, alias) so only the most recent `HISTORY_CAP` remain.
pub async fn insert_history_entry(
    pool: &SqlitePool,
    entry: InsertEntry,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO block_run_history (
            file_path, block_alias, method, url_canonical, status,
            request_size, response_size, elapsed_ms, outcome, ran_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&entry.file_path)
    .bind(&entry.block_alias)
    .bind(&entry.method)
    .bind(&entry.url_canonical)
    .bind(entry.status)
    .bind(entry.request_size)
    .bind(entry.response_size)
    .bind(entry.elapsed_ms)
    .bind(&entry.outcome)
    .bind(&now)
    .execute(pool)
    .await?;

    // Trim: keep the most recent HISTORY_CAP rows for this block.
    sqlx::query(
        "DELETE FROM block_run_history
         WHERE file_path = ? AND block_alias = ?
           AND id NOT IN (
             SELECT id FROM block_run_history
             WHERE file_path = ? AND block_alias = ?
             ORDER BY ran_at DESC
             LIMIT ?
           )",
    )
    .bind(&entry.file_path)
    .bind(&entry.block_alias)
    .bind(&entry.file_path)
    .bind(&entry.block_alias)
    .bind(HISTORY_CAP)
    .execute(pool)
    .await?;

    Ok(())
}

/// Return the most recent N entries for a (file, alias), most recent first.
pub async fn list_history(
    pool: &SqlitePool,
    file_path: &str,
    block_alias: &str,
) -> Result<Vec<HistoryEntry>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (
        i64,
        String,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        String,
    )>(
        "SELECT id, file_path, block_alias, method, url_canonical, status,
                request_size, response_size, elapsed_ms, outcome, ran_at
         FROM block_run_history
         WHERE file_path = ? AND block_alias = ?
         ORDER BY ran_at DESC
         LIMIT ?",
    )
    .bind(file_path)
    .bind(block_alias)
    .bind(HISTORY_CAP)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| HistoryEntry {
            id: r.0,
            file_path: r.1,
            block_alias: r.2,
            method: r.3,
            url_canonical: r.4,
            status: r.5,
            request_size: r.6,
            response_size: r.7,
            elapsed_ms: r.8,
            outcome: r.9,
            ran_at: r.10,
        })
        .collect())
}

/// Delete all history rows for a (file, alias). Called when a block is
/// deleted from the document or a note is removed.
pub async fn purge_history(
    pool: &SqlitePool,
    file_path: &str,
    block_alias: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM block_run_history WHERE file_path = ? AND block_alias = ?",
    )
    .bind(file_path)
    .bind(block_alias)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Apply the migration manually for tests.
        sqlx::query(
            "CREATE TABLE block_run_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT NOT NULL,
                block_alias TEXT NOT NULL,
                method TEXT NOT NULL,
                url_canonical TEXT NOT NULL,
                status INTEGER,
                request_size INTEGER,
                response_size INTEGER,
                elapsed_ms INTEGER,
                outcome TEXT NOT NULL,
                ran_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn entry(method: &str, status: i64) -> InsertEntry {
        InsertEntry {
            file_path: "/notes/test.md".to_string(),
            block_alias: "req1".to_string(),
            method: method.to_string(),
            url_canonical: "https://api.example.com/users".to_string(),
            status: Some(status),
            request_size: Some(0),
            response_size: Some(42),
            elapsed_ms: Some(100),
            outcome: "success".to_string(),
        }
    }

    #[tokio::test]
    async fn inserts_and_lists() {
        let pool = setup().await;
        insert_history_entry(&pool, entry("GET", 200)).await.unwrap();
        insert_history_entry(&pool, entry("POST", 201)).await.unwrap();
        let rows = list_history(&pool, "/notes/test.md", "req1").await.unwrap();
        assert_eq!(rows.len(), 2);
        // Most recent first → POST was inserted last.
        assert_eq!(rows[0].method, "POST");
        assert_eq!(rows[1].method, "GET");
    }

    #[tokio::test]
    async fn trims_to_history_cap() {
        let pool = setup().await;
        for i in 0..15 {
            // Status doubles as an ordinal so we can identify which rows survived.
            insert_history_entry(&pool, entry("GET", 200 + i))
                .await
                .unwrap();
        }
        let rows = list_history(&pool, "/notes/test.md", "req1").await.unwrap();
        assert_eq!(rows.len(), HISTORY_CAP as usize);
        // Newest 10 should be statuses 205..=214.
        let statuses: Vec<i64> = rows.iter().map(|r| r.status.unwrap()).collect();
        assert_eq!(statuses[0], 214);
        assert_eq!(statuses[9], 205);
    }

    #[tokio::test]
    async fn isolates_by_file_and_alias() {
        let pool = setup().await;
        let mut e = entry("GET", 200);
        e.file_path = "/a.md".to_string();
        insert_history_entry(&pool, e).await.unwrap();
        let mut e = entry("GET", 200);
        e.file_path = "/b.md".to_string();
        insert_history_entry(&pool, e).await.unwrap();
        let mut e = entry("GET", 200);
        e.block_alias = "other".to_string();
        insert_history_entry(&pool, e).await.unwrap();

        assert_eq!(list_history(&pool, "/a.md", "req1").await.unwrap().len(), 1);
        assert_eq!(list_history(&pool, "/b.md", "req1").await.unwrap().len(), 1);
        assert_eq!(
            list_history(&pool, "/notes/test.md", "other")
                .await
                .unwrap()
                .len(),
            1,
        );
    }

    #[tokio::test]
    async fn purge_removes_block_history() {
        let pool = setup().await;
        for _ in 0..3 {
            insert_history_entry(&pool, entry("GET", 200)).await.unwrap();
        }
        let removed = purge_history(&pool, "/notes/test.md", "req1")
            .await
            .unwrap();
        assert_eq!(removed, 3);
        let rows = list_history(&pool, "/notes/test.md", "req1").await.unwrap();
        assert!(rows.is_empty());
    }
}
