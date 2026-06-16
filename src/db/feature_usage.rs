use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

/// One aggregated row: how many times `feature` was used on `date`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureUsage {
    pub date: String,
    pub feature: String,
    pub count: i64,
}

/// Increment today's counter for `feature`. Stores only a count — no
/// request/response data, timestamps, or any payload. Aggregation happens
/// in-place via UPSERT so the table stays one row per (day, feature).
pub async fn record_feature_usage(pool: &SqlitePool, feature: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO feature_usage_stats (date, feature, count) \
         VALUES (date('now'), ?, 1) \
         ON CONFLICT (date, feature) DO UPDATE SET count = count + 1",
    )
    .bind(feature)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to record feature usage: {e}"))?;
    Ok(())
}

/// Daily per-feature counts within `[from, to]` (inclusive, ISO dates),
/// ordered by date then feature for stable rendering.
pub async fn get_feature_usage_by_date_range(
    pool: &SqlitePool,
    from: &str,
    to: &str,
) -> Result<Vec<FeatureUsage>, String> {
    let rows = sqlx::query(
        "SELECT date, feature, SUM(count) as count \
         FROM feature_usage_stats WHERE date >= ? AND date <= ? \
         GROUP BY date, feature ORDER BY date ASC, feature ASC",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to get feature usage: {e}"))?;

    Ok(rows
        .iter()
        .map(|r| FeatureUsage {
            date: r.get("date"),
            feature: r.get("feature"),
            count: r.get("count"),
        })
        .collect())
}

/// Delete every recorded event. Backs the "clear usage data" control so a
/// user can reset the local dashboard at any time.
pub async fn clear_feature_usage(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query("DELETE FROM feature_usage_stats")
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to clear feature usage: {e}"))?;
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
    async fn records_and_increments_count() {
        let (pool, _tmp) = setup().await;
        record_feature_usage(&pool, "http_block_run").await.unwrap();
        record_feature_usage(&pool, "http_block_run").await.unwrap();
        record_feature_usage(&pool, "db_block_run").await.unwrap();

        let today: String = sqlx::query_scalar("SELECT date('now')")
            .fetch_one(&pool)
            .await
            .unwrap();
        let rows = get_feature_usage_by_date_range(&pool, &today, &today)
            .await
            .unwrap();

        assert_eq!(rows.len(), 2);
        let http = rows.iter().find(|r| r.feature == "http_block_run").unwrap();
        assert_eq!(http.count, 2);
        let db = rows.iter().find(|r| r.feature == "db_block_run").unwrap();
        assert_eq!(db.count, 1);
    }

    #[tokio::test]
    async fn range_excludes_dates_outside_window() {
        let (pool, _tmp) = setup().await;
        record_feature_usage(&pool, "http_block_run").await.unwrap();

        // A window entirely in the past must return nothing.
        let rows = get_feature_usage_by_date_range(&pool, "2000-01-01", "2000-01-02")
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn clear_removes_all_rows() {
        let (pool, _tmp) = setup().await;
        record_feature_usage(&pool, "http_block_run").await.unwrap();
        clear_feature_usage(&pool).await.unwrap();

        let today: String = sqlx::query_scalar("SELECT date('now')")
            .fetch_one(&pool)
            .await
            .unwrap();
        let rows = get_feature_usage_by_date_range(&pool, &today, &today)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }
}
