//! `DatabasePool` enum + per-driver pool construction (Postgres /
//! MySQL / SQLite).
//!
//! Extracted from `db::connections` (Epic 20a Story 01 — fourth
//! split). Owns the lifecycle pieces — enum definition, ping
//! (`test`), `create_pool` factory, driver-specific
//! `build_*_connect_options` helpers, path/name validation, pool
//! config validation, and connection-error sanitization.
//!
//! The query-execution surface (`execute_query` / `execute_select` /
//! `execute_mutation` dispatchers and the per-driver implementations)
//! still lives in `db::connections` and will move out in a follow-up
//! split (per-driver `pool_exec_*.rs`) — tracked in
//! `tech-debt.md`.

use std::time::Duration;

use super::connections::Connection;

#[allow(clippy::large_enum_variant)]
pub enum DatabasePool {
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
    Sqlite(sqlx::SqlitePool),
}

impl DatabasePool {
    pub async fn test(&self) -> Result<(), String> {
        match self {
            Self::Postgres(pool) => {
                sqlx::query("SELECT 1")
                    .fetch_one(pool)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Self::MySql(pool) => {
                sqlx::query("SELECT 1")
                    .fetch_one(pool)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Self::Sqlite(pool) => {
                sqlx::query("SELECT 1")
                    .fetch_one(pool)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

// --- Pool creation ---

fn build_pg_connect_options(conn: &Connection) -> Result<sqlx::postgres::PgConnectOptions, String> {
    use super::keychain::{conn_password_key, resolve_value, KEYCHAIN_SENTINEL};
    use sqlx::postgres::{PgConnectOptions, PgSslMode};

    let host = conn.host.as_deref().unwrap_or("localhost");
    let port = conn.port.unwrap_or(5432) as u16;
    let db = conn
        .database_name
        .as_deref()
        .ok_or("database_name is required for postgres")?;
    let user = conn.username.as_deref().unwrap_or("postgres");
    let db_password_raw = conn.password.as_deref().unwrap_or("");
    let password = if db_password_raw == KEYCHAIN_SENTINEL {
        resolve_value(db_password_raw, &conn_password_key(&conn.id)).unwrap_or_default()
    } else {
        db_password_raw.to_string()
    };
    let ssl_mode = match conn.ssl_mode.as_deref().unwrap_or("prefer") {
        "require" | "verify-ca" | "verify-full" => PgSslMode::Require,
        "disable" => PgSslMode::Disable,
        _ => PgSslMode::Prefer,
    };

    Ok(PgConnectOptions::new()
        .host(host)
        .port(port)
        .database(db)
        .username(user)
        .password(&password)
        .ssl_mode(ssl_mode))
}

// --- SQLite path validation (T03) ---

pub(super) fn validate_sqlite_path(path: &str) -> Result<(), String> {
    if path == ":memory:" || path.starts_with(":memory:") {
        return Ok(());
    }
    if path.contains("../") || path.contains("..\\") {
        return Err("SQLite path must not contain path traversal (../)".to_string());
    }
    let resolved = std::path::Path::new(path);
    if !resolved.is_absolute() {
        return Err("SQLite database_name must be an absolute path".to_string());
    }
    Ok(())
}

// --- MySQL database name validation (T02) ---

pub(super) fn validate_mysql_database_name(name: &str) -> Result<(), String> {
    if name.contains('`') || name.contains(';') || name.contains('\0') || name.contains('\\') {
        return Err(
            "Database name contains forbidden characters (backtick, semicolon, null, backslash)"
                .to_string(),
        );
    }
    if name.len() > 64 {
        return Err("Database name exceeds MySQL 64-character limit".to_string());
    }
    Ok(())
}

fn build_mysql_connect_options(
    conn: &Connection,
) -> Result<sqlx::mysql::MySqlConnectOptions, String> {
    use super::keychain::{conn_password_key, resolve_value, KEYCHAIN_SENTINEL};
    use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};

    let host = conn.host.as_deref().unwrap_or("localhost");
    let port = conn.port.unwrap_or(3306) as u16;
    let user = conn.username.as_deref().unwrap_or("root");
    let db_password_raw = conn.password.as_deref().unwrap_or("");
    let password = if db_password_raw == KEYCHAIN_SENTINEL {
        resolve_value(db_password_raw, &conn_password_key(&conn.id)).unwrap_or_default()
    } else {
        db_password_raw.to_string()
    };
    let ssl_mode = match conn.ssl_mode.as_deref().unwrap_or("prefer") {
        "require" | "verify-ca" | "verify-full" => MySqlSslMode::Required,
        "disable" => MySqlSslMode::Disabled,
        _ => MySqlSslMode::Preferred,
    };

    // NOTE: intentionally DO NOT call opts.database(db) here. Passing the schema
    // via CLIENT_CONNECT_WITH_DB in the handshake breaks routing in ProxySQL
    // deployments that apply schema-based hostgroup rules on USE/queries only.
    // We select the database via `USE` in after_connect instead.
    Ok(MySqlConnectOptions::new()
        .host(host)
        .port(port)
        .username(user)
        .password(&password)
        .ssl_mode(ssl_mode))
}

pub(super) fn validate_pool_config(conn: &Connection) -> Result<(), String> {
    if conn.max_pool_size < 1 || conn.max_pool_size > 100 {
        return Err(format!(
            "max_pool_size must be between 1 and 100, got {}",
            conn.max_pool_size
        ));
    }
    if conn.timeout_ms < 100 || conn.timeout_ms > 300_000 {
        return Err(format!(
            "timeout_ms must be between 100 and 300000, got {}",
            conn.timeout_ms
        ));
    }
    if conn.query_timeout_ms < 100 || conn.query_timeout_ms > 600_000 {
        return Err(format!(
            "query_timeout_ms must be between 100 and 600000, got {}",
            conn.query_timeout_ms
        ));
    }
    // SQLite connections have no TCP port; skip the range check entirely
    // (older records may have been persisted with `port = 0` by an earlier
    // bug in `update_connection`, and we don't want them stuck unusable).
    if conn.driver != "sqlite" {
        if let Some(port) = conn.port {
            if !(1..=65535).contains(&port) {
                return Err(format!("port must be between 1 and 65535, got {port}"));
            }
        }
    }
    if conn.ttl_seconds < 10 || conn.ttl_seconds > 86400 {
        return Err(format!(
            "ttl_seconds must be between 10 and 86400, got {}",
            conn.ttl_seconds
        ));
    }
    Ok(())
}

/// Sanitize connection errors to prevent leaking credentials in sqlx error messages.
fn sanitize_connection_error(driver: &str, e: sqlx::Error) -> String {
    #[cfg(debug_assertions)]
    eprintln!("[db] {} connection error: {e}", driver);

    match &e {
        sqlx::Error::PoolTimedOut => format!("Connection to {driver} timed out"),
        sqlx::Error::Configuration(_) => format!("Invalid {driver} configuration"),
        _ => format!("Failed to connect to {driver}"),
    }
}

pub(super) async fn create_pool(conn: &Connection) -> Result<DatabasePool, String> {
    validate_pool_config(conn)?;
    let max_conns = conn.max_pool_size as u32;
    let timeout = Duration::from_millis(conn.timeout_ms as u64);

    match conn.driver.as_str() {
        "postgres" => {
            let opts = build_pg_connect_options(conn)?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(max_conns)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await
                .map_err(|e| sanitize_connection_error("postgres", e))?;
            Ok(DatabasePool::Postgres(pool))
        }
        "mysql" => {
            let opts = build_mysql_connect_options(conn)?;
            let db_name = conn.database_name.clone().unwrap_or_default();
            let mut pool_opts = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(max_conns)
                .acquire_timeout(timeout);
            if !db_name.is_empty() {
                validate_mysql_database_name(&db_name)?;
                pool_opts = pool_opts.after_connect(move |conn, _meta| {
                    let db = db_name.clone();
                    Box::pin(async move {
                        use sqlx::Executor;
                        // Pass the SQL as a plain `&str` so sqlx uses the text
                        // protocol (COM_QUERY). Prepared-statement `USE` is
                        // rejected by ProxySQL with error 1295.
                        let sql = format!("USE `{}`", db);
                        conn.execute(sql.as_str()).await?;
                        Ok(())
                    })
                });
            }
            let pool = pool_opts
                .connect_with(opts)
                .await
                .map_err(|e| sanitize_connection_error("mysql", e))?;
            Ok(DatabasePool::MySql(pool))
        }
        "sqlite" => {
            let path = conn
                .database_name
                .as_deref()
                .ok_or("database_name (file path) is required for sqlite")?;
            validate_sqlite_path(path)?;
            let url = format!("sqlite:{path}");
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(max_conns)
                .acquire_timeout(timeout)
                .connect(&url)
                .await
                .map_err(|e| sanitize_connection_error("sqlite", e))?;
            Ok(DatabasePool::Sqlite(pool))
        }
        other => Err(format!("Unsupported driver: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_sqlite_path_accepts_memory() {
        assert!(validate_sqlite_path(":memory:").is_ok());
    }

    #[test]
    fn validate_sqlite_path_rejects_traversal() {
        assert!(validate_sqlite_path("../foo.db").is_err());
        assert!(validate_sqlite_path("..\\foo.db").is_err());
    }

    #[test]
    fn validate_sqlite_path_rejects_relative() {
        assert!(validate_sqlite_path("foo.db").is_err());
    }

    #[test]
    fn validate_mysql_database_name_rejects_unsafe_chars() {
        assert!(validate_mysql_database_name("db`name").is_err());
        assert!(validate_mysql_database_name("db;DROP").is_err());
        assert!(validate_mysql_database_name("db\\name").is_err());
        assert!(validate_mysql_database_name("db\0name").is_err());
    }

    #[test]
    fn validate_mysql_database_name_enforces_length() {
        assert!(validate_mysql_database_name(&"a".repeat(64)).is_ok());
        assert!(validate_mysql_database_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn validate_mysql_database_name_accepts_normal() {
        assert!(validate_mysql_database_name("payments").is_ok());
        assert!(validate_mysql_database_name("payments_v2").is_ok());
    }
}
