//! Inferred response shapes for `{{alias.path}}` resolution. The shape
//! is the structural skeleton of a concrete response value (no data,
//! only types): objects keep their keys, arrays keep one sampled
//! element, scalars become their type name. Written on every successful
//! run; read-only consumers (the language server) resolve ref paths and
//! field typos against it.

use sqlx::SqlitePool;

/// Readers must treat rows with a different version as a cache miss;
/// rebuild happens on the next run, never by migrating rows in place.
pub const CACHE_SCHEMA_VERSION: i64 = 1;

/// Structural skeleton of a JSON value: `{"id": 1}` -> `{"id": "number"}`.
pub fn json_shape(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Null => Value::String("null".into()),
        Value::Bool(_) => Value::String("boolean".into()),
        Value::Number(_) => Value::String("number".into()),
        Value::String(_) => Value::String("string".into()),
        Value::Array(items) => match items.first() {
            Some(first) => Value::Array(vec![json_shape(first)]),
            None => Value::Array(vec![]),
        },
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_shape(v)))
                .collect(),
        ),
    }
}

pub async fn upsert_block_schema(
    pool: &SqlitePool,
    file_path: &str,
    alias: &str,
    response: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    let shape = json_shape(response).to_string();
    sqlx::query(
        "INSERT INTO block_schema_cache (file_path, alias, shape, cache_schema_version, updated_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))
         ON CONFLICT(file_path, alias) DO UPDATE SET
           shape = excluded.shape,
           cache_schema_version = excluded.cache_schema_version,
           updated_at = datetime('now')",
    )
    .bind(file_path)
    .bind(alias)
    .bind(shape)
    .bind(CACHE_SCHEMA_VERSION)
    .execute(pool)
    .await?;
    Ok(())
}

/// Latest shape for `(file_path, alias)`, or `None` when absent or
/// written by a different cache version.
pub async fn get_block_schema(
    pool: &SqlitePool,
    file_path: &str,
    alias: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT shape FROM block_schema_cache
         WHERE file_path = ?1 AND alias = ?2 AND cache_schema_version = ?3",
    )
    .bind(file_path)
    .bind(alias)
    .bind(CACHE_SCHEMA_VERSION)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(shape,)| shape))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shape_of_scalars_and_nesting() {
        let v = json!({"id": 7, "name": "x", "ok": true, "meta": null,
                       "items": [{"sku": "a", "qty": 2}], "empty": []});
        assert_eq!(
            json_shape(&v),
            json!({"id": "number", "name": "string", "ok": "boolean",
                   "meta": "null",
                   "items": [{"sku": "string", "qty": "number"}],
                   "empty": []})
        );
    }

    #[tokio::test]
    async fn upsert_and_read_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pool = crate::db::init_db(tmp.path()).await.unwrap();
        let resp = json!({"body": {"url": "https://x", "n": 1}});
        upsert_block_schema(&pool, "a.md", "req1", &resp)
            .await
            .unwrap();
        // overwrite with a new shape — latest wins
        let resp2 = json!({"body": {"url": "https://x"}});
        upsert_block_schema(&pool, "a.md", "req1", &resp2)
            .await
            .unwrap();
        let shape = get_block_schema(&pool, "a.md", "req1").await.unwrap();
        assert_eq!(shape.as_deref(), Some(r#"{"body":{"url":"string"}}"#));
        assert_eq!(
            get_block_schema(&pool, "a.md", "ghost").await.unwrap(),
            None
        );
    }
}
