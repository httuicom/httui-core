use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
use sqlx::{Column, Row, TypeInfo};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Trait for emitting connection status events.
/// The Tauri app provides an AppHandle-based implementation;
/// the MCP binary (and tests) use None.
pub trait StatusEmitter: Send + Sync {
    fn emit_connection_status(&self, connection_id: &str, name: &str, status: &str);
}

// --- DatabasePool enum ---

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

// --- Connection model ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
    pub timeout_ms: i64,
    pub query_timeout_ms: i64,
    pub ttl_seconds: i64,
    pub max_pool_size: i64,
    pub last_tested_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Public DTO without password field — safe for Tauri IPC responses.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionPublic {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    pub has_password: bool,
    pub ssl_mode: Option<String>,
    pub timeout_ms: i64,
    pub query_timeout_ms: i64,
    pub ttl_seconds: i64,
    pub max_pool_size: i64,
    pub last_tested_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Connection {
    pub fn to_public(&self) -> ConnectionPublic {
        ConnectionPublic {
            id: self.id.clone(),
            name: self.name.clone(),
            driver: self.driver.clone(),
            host: self.host.clone(),
            port: self.port,
            database_name: self.database_name.clone(),
            username: self.username.clone(),
            has_password: self.password.as_ref().map_or(false, |p| !p.is_empty()),
            ssl_mode: self.ssl_mode.clone(),
            timeout_ms: self.timeout_ms,
            query_timeout_ms: self.query_timeout_ms,
            ttl_seconds: self.ttl_seconds,
            max_pool_size: self.max_pool_size,
            last_tested_at: self.last_tested_at.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

pub async fn list_connections_public(pool: &SqlitePool) -> Result<Vec<ConnectionPublic>, String> {
    let conns = list_connections(pool).await?;
    Ok(conns.into_iter().map(|c| c.to_public()).collect())
}

#[derive(Debug, Deserialize)]
pub struct CreateConnection {
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
    pub timeout_ms: Option<i64>,
    pub query_timeout_ms: Option<i64>,
    pub ttl_seconds: Option<i64>,
    pub max_pool_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConnection {
    pub name: Option<String>,
    pub driver: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
    pub timeout_ms: Option<i64>,
    pub query_timeout_ms: Option<i64>,
    pub ttl_seconds: Option<i64>,
    pub max_pool_size: Option<i64>,
}

// --- ConnectionManager ---

pub struct PoolManager {
    app_pool: SqlitePool,
    pools: RwLock<HashMap<String, PoolEntry>>,
    emitter: Option<Arc<dyn StatusEmitter>>,
}

struct PoolEntry {
    pool: Arc<DatabasePool>,
    name: String,
    last_used: Instant,
    ttl_seconds: u64,
    query_timeout_ms: u64,
}

impl PoolManager {
    pub fn new_with_emitter(app_pool: SqlitePool, emitter: Arc<dyn StatusEmitter>) -> Self {
        Self {
            app_pool,
            pools: RwLock::new(HashMap::new()),
            emitter: Some(emitter),
        }
    }

    /// Create without event emitter (for MCP server and tests).
    pub fn new_standalone(app_pool: SqlitePool) -> Self {
        Self {
            app_pool,
            pools: RwLock::new(HashMap::new()),
            emitter: None,
        }
    }

    pub fn app_pool(&self) -> &SqlitePool {
        &self.app_pool
    }

    pub async fn get_pool(&self, connection_id: &str) -> Result<Arc<DatabasePool>, String> {
        // Check cache — write lock to update last_used on hit
        {
            let mut pools = self.pools.write().await;
            if let Some(entry) = pools.get_mut(connection_id) {
                entry.last_used = Instant::now();
                return Ok(entry.pool.clone());
            }
        }

        // Not cached — load connection and create pool
        let conn = get_connection(&self.app_pool, connection_id)
            .await?
            .ok_or_else(|| format!("Connection '{}' not found", connection_id))?;

        let conn_name = conn.name.clone();
        let pool = Arc::new(create_pool(&conn).await?);

        {
            let mut pools = self.pools.write().await;
            pools.insert(
                connection_id.to_string(),
                PoolEntry {
                    pool: pool.clone(),
                    name: conn_name.clone(),
                    last_used: Instant::now(),
                    ttl_seconds: conn.ttl_seconds as u64,
                    query_timeout_ms: conn.query_timeout_ms as u64,
                },
            );
        }

        if let Some(ref emitter) = self.emitter {
            emitter.emit_connection_status(connection_id, &conn_name, "connected");
        }

        Ok(pool)
    }

    pub async fn invalidate(&self, connection_id: &str) {
        let entry_name = {
            let mut pools = self.pools.write().await;
            pools.remove(connection_id).map(|e| e.name)
        };
        if let (Some(name), Some(ref emitter)) = (entry_name, &self.emitter) {
            emitter.emit_connection_status(connection_id, &name, "disconnected");
        }
    }

    pub async fn cleanup_expired(&self) {
        let to_check: Vec<(String, Instant, u64)> = {
            let pools = self.pools.read().await;
            pools
                .iter()
                .map(|(id, entry)| (id.clone(), entry.last_used, entry.ttl_seconds))
                .collect()
        };

        let mut to_remove = Vec::new();
        for (id, last_used, ttl_seconds) in &to_check {
            if last_used.elapsed() > Duration::from_secs(*ttl_seconds) {
                to_remove.push(id.clone());
            }
        }

        if !to_remove.is_empty() {
            let mut pools = self.pools.write().await;
            for id in &to_remove {
                if let Some(entry) = pools.remove(id) {
                    if let Some(ref emitter) = self.emitter {
                        emitter.emit_connection_status(id, &entry.name, "disconnected");
                    }
                }
            }
        }
    }

    /// Returns the connection's query_timeout_ms from the pool cache, if available.
    pub async fn get_query_timeout(&self, connection_id: &str) -> Option<u64> {
        let pools = self.pools.read().await;
        pools.get(connection_id).map(|e| e.query_timeout_ms)
    }

    /// Delete query_log entries older than 30 days or exceeding 50k rows.
    pub async fn cleanup_query_log(&self) {
        let _ = sqlx::query(
            "DELETE FROM query_log WHERE created_at < datetime('now', '-30 days')",
        )
        .execute(&self.app_pool)
        .await;

        // Cap at 50k entries — delete oldest beyond that
        let _ = sqlx::query(
            "DELETE FROM query_log WHERE id NOT IN (SELECT id FROM query_log ORDER BY id DESC LIMIT 50000)",
        )
        .execute(&self.app_pool)
        .await;
    }

    pub async fn test_connection(&self, connection_id: &str) -> Result<(), String> {
        let conn = get_connection(&self.app_pool, connection_id)
            .await?
            .ok_or_else(|| format!("Connection '{}' not found", connection_id))?;

        let pool = create_pool(&conn).await?;
        pool.test().await?;

        // Update last_tested_at
        sqlx::query("UPDATE connections SET last_tested_at = datetime('now') WHERE id = ?")
            .bind(connection_id)
            .execute(&self.app_pool)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }
}

// --- Pool creation ---

fn build_pg_connect_options(
    conn: &Connection,
) -> Result<sqlx::postgres::PgConnectOptions, String> {
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

fn validate_sqlite_path(path: &str) -> Result<(), String> {
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

fn validate_mysql_database_name(name: &str) -> Result<(), String> {
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

fn validate_pool_config(conn: &Connection) -> Result<(), String> {
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
    if let Some(port) = conn.port {
        if port < 1 || port > 65535 {
            return Err(format!("port must be between 1 and 65535, got {port}"));
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

/// Sanitize query errors — expose database error messages (safe) but strip connection details.
pub(crate) fn sanitize_query_error(e: sqlx::Error) -> String {
    match &e {
        sqlx::Error::Database(db_err) => {
            format!("Query failed: {}", db_err.message())
        }
        _ => "Query failed".to_string(),
    }
}

async fn create_pool(conn: &Connection) -> Result<DatabasePool, String> {
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

// --- Row mapping ---

fn row_to_connection(row: &sqlx::sqlite::SqliteRow) -> Connection {
    Connection {
        id: row.get("id"),
        name: row.get("name"),
        driver: row.get("driver"),
        host: row.get("host"),
        port: row.get("port"),
        database_name: row.get("database_name"),
        username: row.get("username"),
        password: row.get("password"),
        ssl_mode: row.get("ssl_mode"),
        timeout_ms: row.get("timeout_ms"),
        query_timeout_ms: row.get("query_timeout_ms"),
        ttl_seconds: row.get("ttl_seconds"),
        max_pool_size: row.get("max_pool_size"),
        last_tested_at: row.get("last_tested_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

// --- CRUD functions ---

pub async fn list_connections(pool: &SqlitePool) -> Result<Vec<Connection>, String> {
    let rows = sqlx::query(
        r#"SELECT
            id, name, driver, host, port, database_name, username, password,
            ssl_mode, timeout_ms, query_timeout_ms, ttl_seconds, max_pool_size,
            last_tested_at, created_at, updated_at
        FROM connections
        ORDER BY name"#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows.iter().map(row_to_connection).collect())
}

pub async fn get_connection(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<Connection>, String> {
    let row = sqlx::query(
        r#"SELECT
            id, name, driver, host, port, database_name, username, password,
            ssl_mode, timeout_ms, query_timeout_ms, ttl_seconds, max_pool_size,
            last_tested_at, created_at, updated_at
        FROM connections WHERE id = ?"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(row.as_ref().map(row_to_connection))
}

pub async fn create_connection(
    pool: &SqlitePool,
    input: CreateConnection,
) -> Result<Connection, String> {
    validate_connection_fields(&input.driver, &input.host, &input.port, &input.database_name)?;

    let id = Uuid::new_v4().to_string();
    let ssl_mode = input.ssl_mode.unwrap_or_else(|| "prefer".to_string());
    let timeout_ms = input.timeout_ms.unwrap_or(10000);
    let query_timeout_ms = input.query_timeout_ms.unwrap_or(30000);
    let ttl_seconds = input.ttl_seconds.unwrap_or(300);
    let max_pool_size = input.max_pool_size.unwrap_or(5);

    // Store password in keychain — fail on error, no plaintext fallback
    let db_password = if let Some(ref pw) = input.password {
        if !pw.is_empty() {
            use super::keychain::{conn_password_key, store_secret, KEYCHAIN_SENTINEL};
            store_secret(&conn_password_key(&id), pw)
                .map_err(|e| format!("Failed to store password securely: {e}"))?;
            Some(KEYCHAIN_SENTINEL.to_string())
        } else {
            input.password.clone()
        }
    } else {
        None
    };

    sqlx::query(
        r#"INSERT INTO connections
            (id, name, driver, host, port, database_name, username, password,
             ssl_mode, timeout_ms, query_timeout_ms, ttl_seconds, max_pool_size)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&id)
    .bind(&input.name)
    .bind(&input.driver)
    .bind(&input.host)
    .bind(&input.port)
    .bind(&input.database_name)
    .bind(&input.username)
    .bind(&db_password)
    .bind(&ssl_mode)
    .bind(timeout_ms)
    .bind(query_timeout_ms)
    .bind(ttl_seconds)
    .bind(max_pool_size)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    get_connection(pool, &id)
        .await?
        .ok_or_else(|| "Failed to fetch created connection".to_string())
}

pub async fn update_connection(
    pool: &SqlitePool,
    id: &str,
    input: UpdateConnection,
) -> Result<Connection, String> {
    let existing = get_connection(pool, id)
        .await?
        .ok_or_else(|| format!("Connection '{}' not found", id))?;

    let name = input.name.unwrap_or(existing.name);
    let driver = input.driver.unwrap_or(existing.driver);
    let host = Some(input.host.unwrap_or_else(|| existing.host.unwrap_or_default()));
    let port = Some(input.port.unwrap_or_else(|| existing.port.unwrap_or(0)));
    let database_name = Some(
        input
            .database_name
            .unwrap_or_else(|| existing.database_name.unwrap_or_default()),
    );
    let username = Some(
        input
            .username
            .unwrap_or_else(|| existing.username.unwrap_or_default()),
    );
    // If a new password is provided, store in keychain — fail on error, no plaintext fallback
    let password = if let Some(ref new_pw) = input.password {
        if !new_pw.is_empty() {
            use super::keychain::{conn_password_key, store_secret, KEYCHAIN_SENTINEL};
            store_secret(&conn_password_key(id), new_pw)
                .map_err(|e| format!("Failed to store password securely: {e}"))?;
            Some(KEYCHAIN_SENTINEL.to_string())
        } else {
            Some(String::new())
        }
    } else {
        existing.password // keep existing (may already be sentinel)
    };
    let ssl_mode = Some(
        input
            .ssl_mode
            .unwrap_or_else(|| existing.ssl_mode.unwrap_or_else(|| "prefer".to_string())),
    );
    let timeout_ms = input.timeout_ms.unwrap_or(existing.timeout_ms);
    let query_timeout_ms = input.query_timeout_ms.unwrap_or(existing.query_timeout_ms);
    let ttl_seconds = input.ttl_seconds.unwrap_or(existing.ttl_seconds);
    let max_pool_size = input.max_pool_size.unwrap_or(existing.max_pool_size);

    validate_connection_fields(&driver, &host, &port, &database_name)?;

    sqlx::query(
        r#"UPDATE connections SET
            name = ?, driver = ?, host = ?, port = ?, database_name = ?,
            username = ?, password = ?, ssl_mode = ?, timeout_ms = ?,
            query_timeout_ms = ?, ttl_seconds = ?, max_pool_size = ?,
            updated_at = datetime('now')
        WHERE id = ?"#,
    )
    .bind(&name)
    .bind(&driver)
    .bind(&host)
    .bind(&port)
    .bind(&database_name)
    .bind(&username)
    .bind(&password)
    .bind(&ssl_mode)
    .bind(timeout_ms)
    .bind(query_timeout_ms)
    .bind(ttl_seconds)
    .bind(max_pool_size)
    .bind(id)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    get_connection(pool, id)
        .await?
        .ok_or_else(|| "Failed to fetch updated connection".to_string())
}

pub async fn delete_connection(pool: &SqlitePool, id: &str) -> Result<(), String> {
    let result = sqlx::query("DELETE FROM connections WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    if result.rows_affected() == 0 {
        return Err(format!("Connection '{}' not found", id));
    }

    // Clean up keychain entry (ignore errors — may not exist)
    use super::keychain::{conn_password_key, delete_secret};
    let _ = delete_secret(&conn_password_key(id));

    Ok(())
}

// --- Validation ---

fn validate_connection_fields(
    driver: &str,
    host: &Option<String>,
    _port: &Option<i64>,
    database_name: &Option<String>,
) -> Result<(), String> {
    match driver {
        "postgres" | "mysql" => {
            if host.as_ref().map_or(true, |h| h.is_empty()) {
                return Err(format!("host is required for {driver}"));
            }
            if database_name.as_ref().map_or(true, |d| d.is_empty()) {
                return Err(format!("database_name is required for {driver}"));
            }
        }
        "sqlite" => {
            if database_name.as_ref().map_or(true, |d| d.is_empty()) {
                return Err("database_name (file path) is required for sqlite".to_string());
            }
            if let Some(ref path) = database_name {
                validate_sqlite_path(path)?;
            }
        }
        other => return Err(format!("Unsupported driver: {other}")),
    }
    Ok(())
}

// --- Query execution helpers (used by DbExecutor in executor/db/) ---

/// Row data as JSON-compatible values.
pub type JsonRow = Vec<serde_json::Value>;

#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<JsonRow>,
    pub has_more: bool,
    pub rows_affected: Option<u64>,
    pub is_select: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
}

impl DatabasePool {
    pub fn driver(&self) -> &str {
        match self {
            Self::Postgres(_) => "postgres",
            Self::MySql(_) => "mysql",
            Self::Sqlite(_) => "sqlite",
        }
    }

    pub async fn execute_query(
        &self,
        sql: &str,
        bind_values: &[serde_json::Value],
        offset: u32,
        fetch_size: u32,
    ) -> Result<QueryResult, String> {
        // T08: Reject multi-statement queries
        if contains_multiple_statements(sql) {
            return Err("Multi-statement queries are not allowed".to_string());
        }

        // T23/T13: Reject non-primitive or out-of-range bind values
        validate_bind_values(bind_values)?;

        // T22: Validate bind count matches placeholder count
        let expected = count_placeholders(sql);
        if bind_values.len() != expected {
            return Err(format!(
                "Bind values count ({}) does not match placeholder count ({expected})",
                bind_values.len()
            ));
        }

        let trimmed = sql.trim_start().to_uppercase();

        // T09: Restrict EXPLAIN ANALYZE with mutation keywords
        if trimmed.starts_with("EXPLAIN") {
            if trimmed.contains("ANALYZE") || trimmed.contains("ANALYSE") {
                let after_explain = trimmed
                    .trim_start_matches("EXPLAIN")
                    .trim()
                    .trim_start_matches("ANALYZE")
                    .trim_start_matches("ANALYSE")
                    .trim_start();
                let mutation_keywords = [
                    "DELETE", "UPDATE", "INSERT", "DROP", "ALTER", "TRUNCATE",
                ];
                if mutation_keywords
                    .iter()
                    .any(|kw| after_explain.starts_with(kw))
                {
                    return Err(
                        "EXPLAIN ANALYZE with mutation statements is not allowed".to_string(),
                    );
                }
            }
        }

        let is_select = if trimmed.starts_with("PRAGMA") {
            // PRAGMA with = is a write operation (e.g. PRAGMA journal_mode=WAL)
            !trimmed.contains('=')
        } else {
            trimmed.starts_with("SELECT")
                || trimmed.starts_with("WITH")
                || trimmed.starts_with("SHOW")
                || trimmed.starts_with("DESCRIBE")
                || trimmed.starts_with("EXPLAIN")
        };

        if is_select {
            self.execute_select(sql, bind_values, offset, fetch_size).await
        } else {
            self.execute_mutation(sql, bind_values).await
        }
    }

    async fn execute_select(
        &self,
        sql: &str,
        bind_values: &[serde_json::Value],
        offset: u32,
        fetch_size: u32,
    ) -> Result<QueryResult, String> {
        match self {
            Self::Sqlite(pool) => {
                execute_select_sqlite(pool, sql, bind_values, offset, fetch_size).await
            }
            Self::Postgres(pool) => {
                execute_select_pg(pool, sql, bind_values, offset, fetch_size).await
            }
            Self::MySql(pool) => {
                execute_select_mysql(pool, sql, bind_values, offset, fetch_size).await
            }
        }
    }

    async fn execute_mutation(
        &self,
        sql: &str,
        bind_values: &[serde_json::Value],
    ) -> Result<QueryResult, String> {
        match self {
            Self::Sqlite(pool) => execute_mutation_sqlite(pool, sql, bind_values).await,
            Self::Postgres(pool) => execute_mutation_pg(pool, sql, bind_values).await,
            Self::MySql(pool) => execute_mutation_mysql(pool, sql, bind_values).await,
        }
    }
}

// --- SQLite execution ---

async fn execute_select_sqlite(
    pool: &sqlx::SqlitePool,
    sql: &str,
    bind_values: &[serde_json::Value],
    offset: u32,
    fetch_size: u32,
) -> Result<QueryResult, String> {
    // Fetch one extra row to detect has_more
    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    let paginated_sql = format!("SELECT * FROM ({sql}) LIMIT {limit} OFFSET {off}");

    let mut query = sqlx::query(&paginated_sql);
    for val in bind_values {
        query = bind_sqlite_value(query, val);
    }

    let mut rows = query
        .fetch_all(pool)
        .await
        .map_err(sanitize_query_error)?;

    let has_more = rows.len() > fetch_size as usize;
    if has_more {
        rows.pop(); // Remove the extra probe row
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

    let json_rows: Vec<JsonRow> = rows
        .iter()
        .map(|row| sqlite_row_to_json(row))
        .collect();

    Ok(QueryResult {
        columns,
        rows: json_rows,
        has_more,
        rows_affected: None,
        is_select: true,
    })
}

async fn execute_mutation_sqlite(
    pool: &sqlx::SqlitePool,
    sql: &str,
    bind_values: &[serde_json::Value],
) -> Result<QueryResult, String> {
    let mut query = sqlx::query(sql);
    for val in bind_values {
        query = bind_sqlite_value(query, val);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(sanitize_query_error)?;

    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        has_more: false,
        rows_affected: Some(result.rows_affected()),
        is_select: false,
    })
}

fn bind_sqlite_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    val: &'q serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    // Non-primitive and out-of-range values already rejected by validate_bind_values
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else {
                query.bind(n.as_f64().unwrap())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        _ => unreachable!("Non-primitive bind values rejected by validate_bind_values"),
    }
}

pub(crate) fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> JsonRow {
    row.columns()
        .iter()
        .map(|col| {
            let idx = col.ordinal();
            // Try types in order: integer, real, text, null
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<f64, _>(idx) {
                serde_json::json!(v)
            } else if let Ok(v) = row.try_get::<String, _>(idx) {
                serde_json::Value::String(v)
            } else if let Ok(v) = row.try_get::<bool, _>(idx) {
                serde_json::Value::Bool(v)
            } else {
                serde_json::Value::Null
            }
        })
        .collect()
}

// --- Postgres execution ---

async fn execute_select_pg(
    pool: &sqlx::PgPool,
    sql: &str,
    bind_values: &[serde_json::Value],
    offset: u32,
    fetch_size: u32,
) -> Result<QueryResult, String> {
    let pg_sql = normalize_placeholders_to_pg(sql);

    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    let paginated_sql = format!(
        "SELECT * FROM ({pg_sql}) AS _p LIMIT {limit} OFFSET {off}"
    );

    let mut query = sqlx::query(&paginated_sql);
    for val in bind_values {
        query = bind_pg_value(query, val);
    }

    let mut rows = query
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

    let json_rows: Vec<JsonRow> = rows.iter().map(|row| pg_row_to_json(row)).collect();

    Ok(QueryResult {
        columns,
        rows: json_rows,
        has_more,
        rows_affected: None,
        is_select: true,
    })
}

async fn execute_mutation_pg(
    pool: &sqlx::PgPool,
    sql: &str,
    bind_values: &[serde_json::Value],
) -> Result<QueryResult, String> {
    let pg_sql = normalize_placeholders_to_pg(sql);
    let mut query = sqlx::query(&pg_sql);
    for val in bind_values {
        query = bind_pg_value(query, val);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(sanitize_query_error)?;

    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        has_more: false,
        rows_affected: Some(result.rows_affected()),
        is_select: false,
    })
}

fn bind_pg_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    val: &'q serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    // Non-primitive and out-of-range values already rejected by validate_bind_values
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else {
                query.bind(n.as_f64().unwrap())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        _ => unreachable!("Non-primitive bind values rejected by validate_bind_values"),
    }
}

fn pg_row_to_json(row: &sqlx::postgres::PgRow) -> JsonRow {
    row.columns()
        .iter()
        .map(|col| {
            let idx = col.ordinal();
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<i32, _>(idx) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<f64, _>(idx) {
                serde_json::json!(v)
            } else if let Ok(v) = row.try_get::<bool, _>(idx) {
                serde_json::Value::Bool(v)
            } else if let Ok(v) = row.try_get::<String, _>(idx) {
                serde_json::Value::String(v)
            } else {
                serde_json::Value::Null
            }
        })
        .collect()
}

// --- MySQL execution ---

async fn execute_select_mysql(
    pool: &sqlx::MySqlPool,
    sql: &str,
    bind_values: &[serde_json::Value],
    offset: u32,
    fetch_size: u32,
) -> Result<QueryResult, String> {
    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    let paginated_sql =
        format!("SELECT * FROM ({sql}) AS _p LIMIT {limit} OFFSET {off}");

    let mut query = sqlx::query(&paginated_sql);
    for val in bind_values {
        query = bind_mysql_value(query, val);
    }

    let mut rows = query
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

    let json_rows: Vec<JsonRow> = rows.iter().map(|row| mysql_row_to_json(row)).collect();

    Ok(QueryResult {
        columns,
        rows: json_rows,
        has_more,
        rows_affected: None,
        is_select: true,
    })
}

async fn execute_mutation_mysql(
    pool: &sqlx::MySqlPool,
    sql: &str,
    bind_values: &[serde_json::Value],
) -> Result<QueryResult, String> {
    let mut query = sqlx::query(sql);
    for val in bind_values {
        query = bind_mysql_value(query, val);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(sanitize_query_error)?;

    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        has_more: false,
        rows_affected: Some(result.rows_affected()),
        is_select: false,
    })
}

fn bind_mysql_value<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    val: &'q serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    // Non-primitive and out-of-range values already rejected by validate_bind_values
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else {
                query.bind(n.as_f64().unwrap())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        _ => unreachable!("Non-primitive bind values rejected by validate_bind_values"),
    }
}

fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow) -> JsonRow {
    row.columns()
        .iter()
        .map(|col| {
            let idx = col.ordinal();
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<i32, _>(idx) {
                serde_json::Value::Number(v.into())
            } else if let Ok(v) = row.try_get::<f64, _>(idx) {
                serde_json::json!(v)
            } else if let Ok(v) = row.try_get::<bool, _>(idx) {
                serde_json::Value::Bool(v)
            } else if let Ok(v) = row.try_get::<String, _>(idx) {
                serde_json::Value::String(v)
            } else if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                // Fallback for VARBINARY/BLOB (e.g. ProxySQL returns VARCHAR
                // columns as VARBINARY). Lossy UTF-8 conversion preserves text
                // content; genuine binary blobs will show replacement chars.
                serde_json::Value::String(String::from_utf8_lossy(&v).into_owned())
            } else {
                serde_json::Value::Null
            }
        })
        .collect()
}

// --- Bind parameter validation (T13, T22, T23) ---

fn validate_bind_values(bind_values: &[serde_json::Value]) -> Result<(), String> {
    for (i, val) in bind_values.iter().enumerate() {
        match val {
            serde_json::Value::Array(_) => {
                return Err(format!(
                    "Bind value at index {i} is an array; only primitive types (null, bool, number, string) are supported"
                ));
            }
            serde_json::Value::Object(_) => {
                return Err(format!(
                    "Bind value at index {i} is an object; only primitive types (null, bool, number, string) are supported"
                ));
            }
            serde_json::Value::Number(n) => {
                if n.as_i64().is_none() && n.as_f64().is_none() {
                    return Err(format!(
                        "Bind value at index {i} is a number outside the supported range (i64/f64)"
                    ));
                }
            }
            _ => {} // Null, Bool, String are fine
        }
    }
    Ok(())
}

// --- SQL-aware scanner (shared by multi-statement detection, placeholder normalization, bind count) ---

struct SqlScanner {
    in_single_quote: bool,
    in_block_comment: bool,
    in_line_comment: bool,
    escape_next: bool,
}

impl SqlScanner {
    fn new() -> Self {
        Self {
            in_single_quote: false,
            in_block_comment: false,
            in_line_comment: false,
            escape_next: false,
        }
    }

    fn is_code(&self) -> bool {
        !self.in_single_quote && !self.in_block_comment && !self.in_line_comment
    }

    /// Advance state by one character. Returns how many chars were consumed (1 or 2).
    fn advance(&mut self, ch: char, next: Option<char>) -> usize {
        if self.escape_next {
            self.escape_next = false;
            return 1;
        }

        if self.in_line_comment {
            if ch == '\n' {
                self.in_line_comment = false;
            }
            return 1;
        }

        if self.in_block_comment {
            if ch == '*' && next == Some('/') {
                self.in_block_comment = false;
                return 2;
            }
            return 1;
        }

        if ch == '\\' && self.in_single_quote {
            self.escape_next = true;
            return 1;
        }

        if ch == '-' && next == Some('-') && !self.in_single_quote {
            self.in_line_comment = true;
            return 1;
        }

        if ch == '/' && next == Some('*') && !self.in_single_quote {
            self.in_block_comment = true;
            return 2;
        }

        if ch == '\'' {
            self.in_single_quote = !self.in_single_quote;
        }

        1
    }
}

// --- Multi-statement detection (T08) ---

pub(crate) fn contains_multiple_statements(sql: &str) -> bool {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut scanner = SqlScanner::new();
    let mut i = 0;

    while i < len {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();

        let was_code = scanner.is_code();
        let consumed = scanner.advance(ch, next);

        if ch == ';' && was_code && scanner.is_code() {
            // Trailing semicolon with only whitespace after is OK
            let rest: String = chars[i + 1..].iter().collect();
            if !rest.trim().is_empty() {
                return true;
            }
            return false;
        }

        i += consumed;
    }
    false
}

// --- Placeholder normalization (T10 fix: handles comments) ---

/// Convert `?` placeholders to `$N` for Postgres.
/// Skips `?` inside string literals, block comments, and line comments.
pub fn normalize_placeholders_to_pg(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(sql.len());
    let mut counter = 0u32;
    let mut scanner = SqlScanner::new();
    let mut i = 0;

    while i < len {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();

        let is_code = scanner.is_code();
        let consumed = scanner.advance(ch, next);

        if ch == '?' && is_code {
            counter += 1;
            result.push('$');
            result.push_str(&counter.to_string());
            i += consumed;
            continue;
        }

        // Push all consumed chars
        for j in 0..consumed {
            if i + j < len {
                result.push(chars[i + j]);
            }
        }
        i += consumed;
    }

    result
}

// --- Placeholder count (for bind_values validation) ---

pub fn count_placeholders(sql: &str) -> usize {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut count = 0;
    let mut scanner = SqlScanner::new();
    let mut i = 0;

    while i < len {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();

        let is_code = scanner.is_code();
        let consumed = scanner.advance(ch, next);

        if ch == '?' && is_code {
            count += 1;
        }

        i += consumed;
    }

    count
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("Failed to create test pool");

        sqlx::query(
            r#"CREATE TABLE connections (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                driver TEXT NOT NULL CHECK (driver IN ('postgres', 'mysql', 'sqlite')),
                host TEXT,
                port INTEGER,
                database_name TEXT,
                username TEXT,
                password TEXT,
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
        .execute(&pool)
        .await
        .expect("Failed to create connections table");

        pool
    }

    #[tokio::test]
    async fn test_create_and_list_connections() {
        let pool = setup_test_pool().await;

        let conn = create_connection(
            &pool,
            CreateConnection {
                name: "test-pg".to_string(),
                driver: "postgres".to_string(),
                host: Some("localhost".to_string()),
                port: Some(5432),
                database_name: Some("testdb".to_string()),
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ssl_mode: None,
                timeout_ms: None,
                query_timeout_ms: None,
                ttl_seconds: None,
                max_pool_size: None,
            },
        )
        .await
        .expect("Failed to create connection");

        assert_eq!(conn.name, "test-pg");
        assert_eq!(conn.driver, "postgres");
        assert_eq!(conn.host.as_deref(), Some("localhost"));

        let all = list_connections(&pool).await.expect("Failed to list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "test-pg");
    }

    #[tokio::test]
    async fn test_update_connection() {
        let pool = setup_test_pool().await;

        let conn = create_connection(
            &pool,
            CreateConnection {
                name: "my-conn".to_string(),
                driver: "postgres".to_string(),
                host: Some("localhost".to_string()),
                port: Some(5432),
                database_name: Some("db1".to_string()),
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

        let updated = update_connection(
            &pool,
            &conn.id,
            UpdateConnection {
                name: Some("renamed".to_string()),
                driver: None,
                host: None,
                port: None,
                database_name: None,
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

        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.driver, "postgres");
    }

    #[tokio::test]
    async fn test_delete_connection() {
        let pool = setup_test_pool().await;

        let conn = create_connection(
            &pool,
            CreateConnection {
                name: "to-delete".to_string(),
                driver: "sqlite".to_string(),
                host: None,
                port: None,
                database_name: Some("/tmp/test.db".to_string()),
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

        delete_connection(&pool, &conn.id).await.unwrap();

        let all = list_connections(&pool).await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn test_validate_postgres_requires_host() {
        let result = validate_connection_fields("postgres", &None, &Some(5432), &Some("db".to_string()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("host is required"));
    }

    #[tokio::test]
    async fn test_validate_sqlite_requires_path() {
        let result = validate_connection_fields("sqlite", &None, &None, &None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("database_name"));
    }

    #[test]
    fn test_normalize_placeholders_to_pg() {
        assert_eq!(
            normalize_placeholders_to_pg("SELECT * FROM users WHERE id = ? AND name = ?"),
            "SELECT * FROM users WHERE id = $1 AND name = $2"
        );
    }

    #[test]
    fn test_normalize_placeholders_ignores_strings() {
        assert_eq!(
            normalize_placeholders_to_pg("SELECT * FROM users WHERE name = '?' AND id = ?"),
            "SELECT * FROM users WHERE name = '?' AND id = $1"
        );
    }

    #[test]
    fn test_build_pg_connect_options_with_special_chars() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("db.example.com".to_string()),
            port: Some(5432),
            database_name: Some("mydb".to_string()),
            username: Some("admin".to_string()),
            password: Some("p@ss:w/rd?&=".to_string()),
            ssl_mode: Some("require".to_string()),
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: 5,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };

        // Builder API handles special chars safely — should not panic
        let opts = build_pg_connect_options(&conn);
        assert!(opts.is_ok());
    }

    #[test]
    fn test_validate_sqlite_path_rejects_traversal() {
        assert!(validate_sqlite_path("../../../etc/passwd").is_err());
        assert!(validate_sqlite_path("/home/user/../etc/passwd").is_err());
        assert!(validate_sqlite_path("..\\..\\windows\\system32").is_err());
    }

    #[test]
    fn test_validate_sqlite_path_rejects_relative() {
        assert!(validate_sqlite_path("data.db").is_err());
        assert!(validate_sqlite_path("subdir/data.db").is_err());
    }

    #[test]
    fn test_validate_sqlite_path_accepts_absolute() {
        assert!(validate_sqlite_path("/home/user/data.db").is_ok());
        assert!(validate_sqlite_path("/tmp/test.db").is_ok());
    }

    #[test]
    fn test_validate_sqlite_path_accepts_memory() {
        assert!(validate_sqlite_path(":memory:").is_ok());
    }

    #[test]
    fn test_validate_mysql_db_name_rejects_backtick() {
        assert!(validate_mysql_database_name("db`; DROP TABLE users").is_err());
    }

    #[test]
    fn test_validate_mysql_db_name_rejects_semicolon() {
        assert!(validate_mysql_database_name("db; DROP TABLE").is_err());
    }

    #[test]
    fn test_validate_mysql_db_name_rejects_null_byte() {
        assert!(validate_mysql_database_name("db\0name").is_err());
    }

    #[test]
    fn test_validate_mysql_db_name_rejects_long_name() {
        let name = "a".repeat(65);
        assert!(validate_mysql_database_name(&name).is_err());
    }

    #[test]
    fn test_validate_mysql_db_name_accepts_valid() {
        assert!(validate_mysql_database_name("my_database").is_ok());
        assert!(validate_mysql_database_name("app-prod-db").is_ok());
    }

    #[tokio::test]
    async fn test_execute_select_sqlite() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query("CREATE TABLE test_table (id INTEGER PRIMARY KEY, name TEXT, value REAL)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO test_table VALUES (1, 'alpha', 1.5), (2, 'beta', 2.5), (3, 'gamma', 3.5)")
            .execute(&pool)
            .await
            .unwrap();

        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("SELECT * FROM test_table", &[], 0, 100)
            .await
            .unwrap();

        assert!(result.is_select);
        assert_eq!(result.has_more, false);
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.columns.len(), 3);
        assert_eq!(result.columns[0].name, "id");
        assert_eq!(result.columns[1].name, "name");
    }

    #[tokio::test]
    async fn test_execute_select_with_pagination() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query("CREATE TABLE items (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        for i in 1..=10 {
            sqlx::query("INSERT INTO items VALUES (?)")
                .bind(i)
                .execute(&pool)
                .await
                .unwrap();
        }

        let db_pool = DatabasePool::Sqlite(pool);

        // offset=0, fetch_size=3 → 3 rows, has_more=true
        let result = db_pool
            .execute_query("SELECT * FROM items", &[], 0, 3)
            .await
            .unwrap();
        assert_eq!(result.has_more, true);
        assert_eq!(result.rows.len(), 3);

        // offset=9, fetch_size=3 → 1 row, has_more=false
        let result = db_pool
            .execute_query("SELECT * FROM items", &[], 9, 3)
            .await
            .unwrap();
        assert_eq!(result.has_more, false);
        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn test_execute_mutation_sqlite() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)")
            .execute(&pool)
            .await
            .unwrap();

        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query(
                "INSERT INTO items VALUES (1, 'test')",
                &[],
                1,
                100,
            )
            .await
            .unwrap();

        assert!(!result.is_select);
        assert_eq!(result.rows_affected, Some(1));
    }

    #[tokio::test]
    async fn test_execute_with_bind_params() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INTEGER)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO users VALUES (1, 'alice', 1), (2, 'bob', 0), (3, 'charlie', 1)")
            .execute(&pool)
            .await
            .unwrap();

        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query(
                "SELECT * FROM users WHERE active = ? AND name != ?",
                &[serde_json::json!(1), serde_json::json!("alice")],
                0,
                100,
            )
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][1], serde_json::json!("charlie"));
    }

    // --- Phase 2: Pool config validation tests ---

    #[test]
    fn test_validate_pool_config_rejects_zero_pool_size() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("localhost".to_string()),
            port: Some(5432),
            database_name: Some("db".to_string()),
            username: None,
            password: None,
            ssl_mode: None,
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: 0,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(validate_pool_config(&conn).is_err());
    }

    #[test]
    fn test_validate_pool_config_rejects_negative_pool_size() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("localhost".to_string()),
            port: Some(5432),
            database_name: Some("db".to_string()),
            username: None,
            password: None,
            ssl_mode: None,
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: -5,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(validate_pool_config(&conn).is_err());
    }

    #[test]
    fn test_validate_pool_config_rejects_huge_pool_size() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("localhost".to_string()),
            port: Some(5432),
            database_name: Some("db".to_string()),
            username: None,
            password: None,
            ssl_mode: None,
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: 200,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(validate_pool_config(&conn).is_err());
    }

    #[test]
    fn test_validate_pool_config_rejects_invalid_port() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("localhost".to_string()),
            port: Some(0),
            database_name: Some("db".to_string()),
            username: None,
            password: None,
            ssl_mode: None,
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: 5,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(validate_pool_config(&conn).is_err());

        let conn2 = Connection { port: Some(70000), ..conn };
        assert!(validate_pool_config(&conn2).is_err());
    }

    #[test]
    fn test_validate_pool_config_accepts_valid() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("localhost".to_string()),
            port: Some(5432),
            database_name: Some("db".to_string()),
            username: None,
            password: None,
            ssl_mode: None,
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: 10,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(validate_pool_config(&conn).is_ok());
    }

    // --- Phase 3: Query execution hardening tests ---

    #[test]
    fn test_reject_multi_statement() {
        assert!(contains_multiple_statements("SELECT 1; DROP TABLE users"));
        assert!(contains_multiple_statements("SELECT 1;SELECT 2"));
    }

    #[test]
    fn test_allow_trailing_semicolon() {
        assert!(!contains_multiple_statements("SELECT 1;"));
        assert!(!contains_multiple_statements("SELECT 1;  "));
        assert!(!contains_multiple_statements("SELECT 1;\n"));
    }

    #[test]
    fn test_allow_semicolon_in_string() {
        assert!(!contains_multiple_statements("SELECT * FROM t WHERE v = 'a;b'"));
    }

    #[test]
    fn test_allow_semicolon_in_line_comment() {
        assert!(!contains_multiple_statements("SELECT 1 -- ; comment"));
    }

    #[test]
    fn test_allow_semicolon_in_block_comment() {
        assert!(!contains_multiple_statements("SELECT 1 /* ; */ FROM t"));
    }

    #[tokio::test]
    async fn test_explain_analyze_delete_rejected() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)").execute(&pool).await.unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("EXPLAIN ANALYZE DELETE FROM t", &[], 0, 100)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("EXPLAIN ANALYZE"));
    }

    #[tokio::test]
    async fn test_explain_select_not_blocked() {
        // EXPLAIN SELECT passes the security check (no ANALYZE + mutation).
        // SQLite can't subquery-wrap EXPLAIN output, so it may fail at execution,
        // but the error must NOT be our security rejection.
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)").execute(&pool).await.unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("EXPLAIN SELECT * FROM t", &[], 0, 100)
            .await;
        if let Err(ref e) = result {
            assert!(!e.contains("EXPLAIN ANALYZE"), "Should not be blocked by security check");
            assert!(!e.contains("Multi-statement"), "Should not be blocked by multi-statement check");
        }
    }

    #[tokio::test]
    async fn test_explain_analyze_select_not_blocked() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)").execute(&pool).await.unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("EXPLAIN ANALYZE SELECT * FROM t", &[], 0, 100)
            .await;
        // May fail at execution level, but must NOT be our security rejection
        if let Err(ref e) = result {
            assert!(!e.contains("mutation"), "SELECT should not be blocked as mutation");
        }
    }

    #[tokio::test]
    async fn test_pragma_write_treated_as_mutation() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("PRAGMA journal_mode=WAL", &[], 0, 100)
            .await
            .unwrap();
        assert!(!result.is_select);
    }

    #[tokio::test]
    async fn test_pragma_read_not_blocked() {
        // PRAGMA without = should be classified as SELECT.
        // Subquery wrapping may fail in SQLite, but security check must pass.
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("PRAGMA table_info('sqlite_master')", &[], 0, 100)
            .await;
        if let Err(ref e) = result {
            assert!(!e.contains("Multi-statement"), "Should not be blocked");
        }
    }

    #[test]
    fn test_normalize_ignores_block_comment() {
        assert_eq!(
            normalize_placeholders_to_pg("SELECT /* ? */ * FROM t WHERE id = ?"),
            "SELECT /* ? */ * FROM t WHERE id = $1"
        );
    }

    #[test]
    fn test_normalize_ignores_line_comment() {
        assert_eq!(
            normalize_placeholders_to_pg("SELECT * -- ?\nFROM t WHERE id = ?"),
            "SELECT * -- ?\nFROM t WHERE id = $1"
        );
    }

    #[test]
    fn test_count_placeholders_ignores_string() {
        assert_eq!(count_placeholders("SELECT * FROM t WHERE v = '?' AND id = ?"), 1);
    }

    #[test]
    fn test_count_placeholders_ignores_comments() {
        assert_eq!(count_placeholders("SELECT * -- ?\nFROM t WHERE id = ?"), 1);
        assert_eq!(count_placeholders("SELECT /* ? */ * FROM t WHERE id = ?"), 1);
    }

    #[test]
    fn test_count_placeholders_multiple() {
        assert_eq!(count_placeholders("SELECT * FROM t WHERE a = ? AND b = ? AND c = ?"), 3);
    }

    #[tokio::test]
    async fn test_multi_statement_rejected_in_execute() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)").execute(&pool).await.unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("SELECT 1; DROP TABLE t", &[], 0, 100)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Multi-statement"));
    }

    // --- Phase 4: Bind parameter safety tests ---

    #[test]
    fn test_validate_bind_rejects_array() {
        let vals = vec![serde_json::json!(1), serde_json::json!([2, 3])];
        let result = validate_bind_values(&vals);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("array"));
    }

    #[test]
    fn test_validate_bind_rejects_object() {
        let vals = vec![serde_json::json!({"key": "val"})];
        let result = validate_bind_values(&vals);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("object"));
    }

    #[test]
    fn test_validate_bind_accepts_primitives() {
        let vals = vec![
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!(3.14),
            serde_json::json!("hello"),
        ];
        assert!(validate_bind_values(&vals).is_ok());
    }

    #[tokio::test]
    async fn test_bind_count_mismatch_too_few() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (a INTEGER, b INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query(
                "SELECT * FROM t WHERE a = ? AND b = ?",
                &[serde_json::json!(1)],
                0,
                100,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not match"));
    }

    #[tokio::test]
    async fn test_bind_count_mismatch_too_many() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (a INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query(
                "SELECT * FROM t WHERE a = ?",
                &[serde_json::json!(1), serde_json::json!(2), serde_json::json!(3)],
                0,
                100,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not match"));
    }

    #[tokio::test]
    async fn test_bind_count_matches() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (a INTEGER, b TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (1, 'x'), (2, 'y')")
            .execute(&pool)
            .await
            .unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query(
                "SELECT * FROM t WHERE a = ? AND b = ?",
                &[serde_json::json!(1), serde_json::json!("x")],
                0,
                100,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().rows.len(), 1);
    }
}
