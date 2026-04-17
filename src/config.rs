use sqlx::sqlite::SqlitePool;
use sqlx::Row;

pub async fn get_config(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT value FROM app_config WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| r.get("value")))
}

pub async fn set_config(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO app_config (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn delete_config(pool: &SqlitePool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM app_config WHERE key = ?1")
        .bind(key)
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
    async fn test_get_config_returns_none_for_missing_key() {
        let (pool, _tmp) = setup().await;
        let result = get_config(&pool, "nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_set_and_get_config() {
        let (pool, _tmp) = setup().await;

        set_config(&pool, "theme", "dark").await.unwrap();
        let value = get_config(&pool, "theme").await.unwrap();
        assert_eq!(value, Some("dark".to_string()));
    }

    #[tokio::test]
    async fn test_set_config_upserts() {
        let (pool, _tmp) = setup().await;

        set_config(&pool, "theme", "dark").await.unwrap();
        set_config(&pool, "theme", "light").await.unwrap();

        let value = get_config(&pool, "theme").await.unwrap();
        assert_eq!(value, Some("light".to_string()));
    }

    #[tokio::test]
    async fn test_delete_config() {
        let (pool, _tmp) = setup().await;

        set_config(&pool, "theme", "dark").await.unwrap();
        delete_config(&pool, "theme").await.unwrap();

        let value = get_config(&pool, "theme").await.unwrap();
        assert!(value.is_none());
    }
}
