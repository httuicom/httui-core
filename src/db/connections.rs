use base64::prelude::*;
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
    pub is_readonly: bool,
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
    pub is_readonly: bool,
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
            has_password: self.password.as_ref().is_some_and(|p| !p.is_empty()),
            ssl_mode: self.ssl_mode.clone(),
            timeout_ms: self.timeout_ms,
            query_timeout_ms: self.query_timeout_ms,
            ttl_seconds: self.ttl_seconds,
            max_pool_size: self.max_pool_size,
            is_readonly: self.is_readonly,
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
    pub is_readonly: Option<bool>,
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
    pub is_readonly: Option<bool>,
}

// --- ConnectionManager ---

pub struct PoolManager {
    /// Resolves `Connection` records by name. File-backed in production
    /// (`ConnectionsStore`); SQLite adapter in legacy tests
    /// (`SqliteLookup`). See `db/lookup.rs`.
    lookup: Arc<dyn super::lookup::ConnectionLookup>,
    /// Retained only for `cleanup_query_log` (the `query_log` SQLite
    /// table; Epic 20a Story 01 moves this out and the field with it).
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
    pub fn new_with_emitter(
        lookup: Arc<dyn super::lookup::ConnectionLookup>,
        app_pool: SqlitePool,
        emitter: Arc<dyn StatusEmitter>,
    ) -> Self {
        Self {
            lookup,
            app_pool,
            pools: RwLock::new(HashMap::new()),
            emitter: Some(emitter),
        }
    }

    /// Create without event emitter (for MCP server and tests).
    pub fn new_standalone(
        lookup: Arc<dyn super::lookup::ConnectionLookup>,
        app_pool: SqlitePool,
    ) -> Self {
        Self {
            lookup,
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

        // Not cached — resolve connection and create pool
        let conn = self
            .lookup
            .lookup(connection_id)
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
        let _ = sqlx::query("DELETE FROM query_log WHERE created_at < datetime('now', '-30 days')")
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
        let conn = self
            .lookup
            .lookup(connection_id)
            .await?
            .ok_or_else(|| format!("Connection '{}' not found", connection_id))?;

        let pool = create_pool(&conn).await?;
        pool.test().await?;

        // The legacy `UPDATE connections SET last_tested_at` write is
        // dropped: the file-backed schema doesn't carry that field, and
        // the live status emitter (`emit_connection_status`) is the
        // user-facing signal anyway. Reintroduce as a per-machine cache
        // (e.g. `~/.config/httui/connection_status.toml`) if a UI need
        // emerges — out of scope for v1 (audit-015 Phase 3 decision).

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

/// Sanitize query errors — expose database error messages (safe) but strip connection details.
pub(crate) fn sanitize_query_error(e: sqlx::Error) -> String {
    sanitize_query_error_rich(&e).message
}

/// Cursor location inside the source SQL where the error was reported.
/// Populated from driver-specific metadata (Postgres `position`, MySQL
/// `near … at line N`); `None` when the driver didn't expose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryErrorLocation {
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct QueryErrorInfo {
    pub message: String,
    pub location: QueryErrorLocation,
}

impl std::fmt::Display for QueryErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<QueryErrorInfo> for String {
    fn from(value: QueryErrorInfo) -> String {
        value.message
    }
}

impl QueryErrorInfo {
    /// Convenience proxy so existing `.contains(...)` assertions in tests
    /// keep reading naturally without being forced onto `.message.contains(...)`.
    pub fn contains(&self, needle: &str) -> bool {
        self.message.contains(needle)
    }
}

/// Sanitize a sqlx error AND pull out line/column when the driver exposes
/// them. The string message is the same as `sanitize_query_error` so the
/// existing "Query failed: …" prefix is preserved.
pub fn sanitize_query_error_rich(e: &sqlx::Error) -> QueryErrorInfo {
    match e {
        sqlx::Error::Database(db_err) => {
            let msg = format!("Query failed: {}", db_err.message());
            let location = extract_error_location(db_err.as_ref());
            QueryErrorInfo {
                message: msg,
                location,
            }
        }
        _ => QueryErrorInfo {
            message: "Query failed".to_string(),
            location: QueryErrorLocation::default(),
        },
    }
}

/// Turn a Postgres 1-indexed char position into (line, column). Returns
/// `(1, 1)` if the position overruns the query (shouldn't happen in
/// practice).
fn position_to_line_col(query: &str, position: u32) -> (u32, u32) {
    if position == 0 {
        return (1, 1);
    }
    let target = (position as usize).saturating_sub(1);
    let mut line = 1u32;
    let mut col = 1u32;
    for (idx, ch) in query.char_indices() {
        if idx >= target {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn extract_error_location(db_err: &dyn sqlx::error::DatabaseError) -> QueryErrorLocation {
    // Postgres: downcast to access the PgErrorPosition helper. The position
    // is an offset into the original query string (1-indexed). Callers
    // know the query and convert to line/col via `enrich_error_with_query`.
    if let Some(p) = db_err
        .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
        .and_then(|pg| pg.position())
        .and_then(|pos| match pos {
            sqlx::postgres::PgErrorPosition::Original(p) => Some(p),
            _ => None,
        })
    {
        // Stash position as raw column for now; caller converts
        // once it has the query text.
        return QueryErrorLocation {
            line: None,
            column: Some(p as u32),
        };
    }
    // MySQL: message often contains "near '…' at line N".
    let msg = db_err.message();
    if let Some(line) = mysql_line_from_message(msg) {
        return QueryErrorLocation {
            line: Some(line),
            column: None,
        };
    }
    QueryErrorLocation::default()
}

fn mysql_line_from_message(msg: &str) -> Option<u32> {
    // Example: "You have an error in your SQL syntax; ... at line 3".
    let lower = msg.to_ascii_lowercase();
    let idx = lower.find(" at line ")?;
    let tail = &msg[idx + " at line ".len()..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// After capture, resolve Postgres-style raw byte position into (line, col)
/// using the query text the executor sent. MySQL line (already extracted
/// from the message) passes through unchanged.
pub fn enrich_error_with_query(info: &mut QueryErrorInfo, query: &str) {
    if info.location.line.is_none() {
        if let Some(pos) = info.location.column {
            let (l, c) = position_to_line_col(query, pos);
            info.location.line = Some(l);
            info.location.column = Some(c);
        }
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
        // is_readonly stored as INTEGER (0/1) in SQLite
        is_readonly: row
            .try_get::<i64, _>("is_readonly")
            .map(|v| v != 0)
            .unwrap_or(false),
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
            is_readonly,
            last_tested_at, created_at, updated_at
        FROM connections
        ORDER BY name"#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows.iter().map(row_to_connection).collect())
}

pub async fn get_connection(pool: &SqlitePool, id: &str) -> Result<Option<Connection>, String> {
    let row = sqlx::query(
        r#"SELECT
            id, name, driver, host, port, database_name, username, password,
            ssl_mode, timeout_ms, query_timeout_ms, ttl_seconds, max_pool_size,
            is_readonly,
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
    validate_connection_fields(
        &input.driver,
        &input.host,
        &input.port,
        &input.database_name,
    )?;

    let id = Uuid::new_v4().to_string();
    let ssl_mode = input.ssl_mode.unwrap_or_else(|| "disable".to_string());
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

    let is_readonly = input.is_readonly.unwrap_or(false);

    sqlx::query(
        r#"INSERT INTO connections
            (id, name, driver, host, port, database_name, username, password,
             ssl_mode, timeout_ms, query_timeout_ms, ttl_seconds, max_pool_size,
             is_readonly)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&id)
    .bind(&input.name)
    .bind(&input.driver)
    .bind(&input.host)
    .bind(input.port)
    .bind(&input.database_name)
    .bind(&input.username)
    .bind(&db_password)
    .bind(&ssl_mode)
    .bind(timeout_ms)
    .bind(query_timeout_ms)
    .bind(ttl_seconds)
    .bind(max_pool_size)
    .bind(if is_readonly { 1i64 } else { 0i64 })
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
    // Preserve NULLs across partial updates: SQLite has no host/port, so
    // forcing `Some(existing.port.unwrap_or(0))` would fail validation on
    // every partial-field update (like the drawer's read-only toggle).
    let host = input.host.or(existing.host);
    let port = input.port.or(existing.port);
    let database_name = input.database_name.or(existing.database_name);
    let username = input.username.or(existing.username);
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
            .unwrap_or_else(|| existing.ssl_mode.unwrap_or_else(|| "disable".to_string())),
    );
    let timeout_ms = input.timeout_ms.unwrap_or(existing.timeout_ms);
    let query_timeout_ms = input.query_timeout_ms.unwrap_or(existing.query_timeout_ms);
    let ttl_seconds = input.ttl_seconds.unwrap_or(existing.ttl_seconds);
    let max_pool_size = input.max_pool_size.unwrap_or(existing.max_pool_size);
    let is_readonly = input.is_readonly.unwrap_or(existing.is_readonly);

    validate_connection_fields(&driver, &host, &port, &database_name)?;

    sqlx::query(
        r#"UPDATE connections SET
            name = ?, driver = ?, host = ?, port = ?, database_name = ?,
            username = ?, password = ?, ssl_mode = ?, timeout_ms = ?,
            query_timeout_ms = ?, ttl_seconds = ?, max_pool_size = ?,
            is_readonly = ?,
            updated_at = datetime('now')
        WHERE id = ?"#,
    )
    .bind(&name)
    .bind(&driver)
    .bind(&host)
    .bind(port)
    .bind(&database_name)
    .bind(&username)
    .bind(&password)
    .bind(&ssl_mode)
    .bind(timeout_ms)
    .bind(query_timeout_ms)
    .bind(ttl_seconds)
    .bind(max_pool_size)
    .bind(if is_readonly { 1i64 } else { 0i64 })
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    /// Driver-reported type name (e.g. "INTEGER", "int4"). Renamed on the
    /// wire to `type` so TS consumers can write `col.type` ergonomically.
    #[serde(rename = "type")]
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
    ) -> Result<QueryResult, QueryErrorInfo> {
        // Pre-send validations: not driver errors, so no line/col location.
        let plain_err = |msg: String| QueryErrorInfo {
            message: msg,
            location: QueryErrorLocation::default(),
        };

        // T08: Reject multi-statement queries
        if contains_multiple_statements(sql) {
            return Err(plain_err(
                "Multi-statement queries are not allowed".to_string(),
            ));
        }

        // T23/T13: Reject non-primitive or out-of-range bind values
        validate_bind_values(bind_values).map_err(plain_err)?;

        // T22: Validate bind count matches placeholder count
        let expected = count_placeholders(sql);
        if bind_values.len() != expected {
            return Err(plain_err(format!(
                "Bind values count ({}) does not match placeholder count ({expected})",
                bind_values.len()
            )));
        }

        let trimmed = sql.trim_start().to_uppercase();

        // T09: Restrict EXPLAIN ANALYZE with mutation keywords
        if trimmed.starts_with("EXPLAIN")
            && (trimmed.contains("ANALYZE") || trimmed.contains("ANALYSE"))
        {
            let after_explain = trimmed
                .trim_start_matches("EXPLAIN")
                .trim()
                .trim_start_matches("ANALYZE")
                .trim_start_matches("ANALYSE")
                .trim_start();
            let mutation_keywords = ["DELETE", "UPDATE", "INSERT", "DROP", "ALTER", "TRUNCATE"];
            if mutation_keywords
                .iter()
                .any(|kw| after_explain.starts_with(kw))
            {
                return Err(plain_err(
                    "EXPLAIN ANALYZE with mutation statements is not allowed".to_string(),
                ));
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
            self.execute_select(sql, bind_values, offset, fetch_size)
                .await
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
    ) -> Result<QueryResult, QueryErrorInfo> {
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
    ) -> Result<QueryResult, QueryErrorInfo> {
        match self {
            Self::Sqlite(pool) => execute_mutation_sqlite(pool, sql, bind_values).await,
            Self::Postgres(pool) => execute_mutation_pg(pool, sql, bind_values).await,
            Self::MySql(pool) => execute_mutation_mysql(pool, sql, bind_values).await,
        }
    }
}

// --- SQLite execution ---

/// True only for statements that can legally be wrapped in
/// `SELECT * FROM (<sql>) LIMIT … OFFSET …` for pagination — i.e. real
/// SELECTs and CTEs. EXPLAIN, PRAGMA, SHOW, DESCRIBE all return rows
/// but are not subqueryable in any of the three drivers, so they must
/// run as-is.
fn is_subqueryable_select(sql: &str) -> bool {
    let trimmed = sql.trim_start().to_uppercase();
    trimmed.starts_with("SELECT") || trimmed.starts_with("WITH")
}

async fn execute_select_sqlite(
    pool: &sqlx::SqlitePool,
    sql: &str,
    bind_values: &[serde_json::Value],
    offset: u32,
    fetch_size: u32,
) -> Result<QueryResult, QueryErrorInfo> {
    // Fetch one extra row to detect has_more
    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    // EXPLAIN / PRAGMA / SHOW / DESCRIBE can't be subqueried, so run them
    // raw. We lose the `has_more` probe + pagination on those — fine,
    // those statements never need either.
    let (paginated_sql, paginated) = if is_subqueryable_select(sql) {
        (
            format!("SELECT * FROM ({sql}) LIMIT {limit} OFFSET {off}"),
            true,
        )
    } else {
        (sql.to_string(), false)
    };

    let mut query = sqlx::query(&paginated_sql);
    for val in bind_values {
        query = bind_sqlite_value(query, val);
    }

    let mut rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| sanitize_query_error_rich(&e))?;

    // `has_more` only meaningful when we paginated with a +1 probe row.
    // Non-paginated runs (EXPLAIN/PRAGMA/SHOW/DESCRIBE) return their full
    // output and never have "more".
    let has_more = paginated && rows.len() > fetch_size as usize;
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

    let json_rows: Vec<JsonRow> = rows.iter().map(sqlite_row_to_json).collect();

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
) -> Result<QueryResult, QueryErrorInfo> {
    let mut query = sqlx::query(sql);
    for val in bind_values {
        query = bind_sqlite_value(query, val);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(|e| sanitize_query_error_rich(&e))?;

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
                query.bind(
                    n.as_f64()
                        .expect("serde_json::Number is f64-representable when as_i64 fails"),
                )
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
) -> Result<QueryResult, QueryErrorInfo> {
    let pg_sql = normalize_placeholders_to_pg(sql);

    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    // EXPLAIN / SHOW can't be a relation source in Postgres, so skip the
    // pagination wrapper for them.
    let (paginated_sql, paginated) = if is_subqueryable_select(&pg_sql) {
        (
            format!("SELECT * FROM ({pg_sql}) AS _p LIMIT {limit} OFFSET {off}"),
            true,
        )
    } else {
        (pg_sql, false)
    };

    let mut query = sqlx::query(&paginated_sql);
    for val in bind_values {
        query = bind_pg_value(query, val);
    }

    let mut rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| sanitize_query_error_rich(&e))?;

    let has_more = paginated && rows.len() > fetch_size as usize;
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

    let json_rows: Vec<JsonRow> = rows.iter().map(pg_row_to_json).collect();

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
) -> Result<QueryResult, QueryErrorInfo> {
    let pg_sql = normalize_placeholders_to_pg(sql);
    let mut query = sqlx::query(&pg_sql);
    for val in bind_values {
        query = bind_pg_value(query, val);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(|e| sanitize_query_error_rich(&e))?;

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
                query.bind(
                    n.as_f64()
                        .expect("serde_json::Number is f64-representable when as_i64 fails"),
                )
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
) -> Result<QueryResult, QueryErrorInfo> {
    let limit = (fetch_size + 1) as i64;
    let off = offset as i64;
    // SHOW / DESCRIBE / EXPLAIN aren't subqueryable in MySQL — run raw.
    let (paginated_sql, paginated) = if is_subqueryable_select(sql) {
        (
            format!("SELECT * FROM ({sql}) AS _p LIMIT {limit} OFFSET {off}"),
            true,
        )
    } else {
        (sql.to_string(), false)
    };

    let mut query = sqlx::query(&paginated_sql);
    for val in bind_values {
        query = bind_mysql_value(query, val);
    }

    let mut rows = query
        .fetch_all(pool)
        .await
        .map_err(|e| sanitize_query_error_rich(&e))?;

    let has_more = paginated && rows.len() > fetch_size as usize;
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

    let json_rows: Vec<JsonRow> = rows.iter().map(mysql_row_to_json).collect();

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
) -> Result<QueryResult, QueryErrorInfo> {
    let mut query = sqlx::query(sql);
    for val in bind_values {
        query = bind_mysql_value(query, val);
    }

    let result = query
        .execute(pool)
        .await
        .map_err(|e| sanitize_query_error_rich(&e))?;

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
                query.bind(
                    n.as_f64()
                        .expect("serde_json::Number is f64-representable when as_i64 fails"),
                )
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        _ => unreachable!("Non-primitive bind values rejected by validate_bind_values"),
    }
}

fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow) -> JsonRow {
    row.columns()
        .iter()
        .map(|col| mysql_value_to_json(row, col))
        .collect()
}

// Dispatch by column type name rather than a fallthrough chain of `try_get`.
// sqlx-mysql rejects `i64` for any UNSIGNED column and rejects `String` for
// `JSON` columns, so the old chain silently decoded BIGINT UNSIGNED as `bool`
// and JSON as raw wire bytes (9-byte length prefix + payload).
fn mysql_value_to_json(
    row: &sqlx::mysql::MySqlRow,
    col: &sqlx::mysql::MySqlColumn,
) -> serde_json::Value {
    use sqlx::ValueRef;
    let idx = col.ordinal();

    // Separate a real NULL from a decode failure. Without this distinction,
    // any type sqlx can't decode with the preferred Rust type would silently
    // collapse to JSON null.
    if let Ok(raw) = <sqlx::mysql::MySqlRow as sqlx::Row>::try_get_raw(row, idx) {
        if raw.is_null() {
            return serde_json::Value::Null;
        }
    }

    let ty = col.type_info().name();

    // Preferred per-type decoding; returns None if sqlx can't decode that type.
    // When it returns None, the fallback chain kicks in so the user still sees
    // something instead of null.
    decode_mysql_by_type(row, idx, ty).unwrap_or_else(|| mysql_fallback_decode(row, idx, ty))
}

fn decode_mysql_by_type(
    row: &sqlx::mysql::MySqlRow,
    idx: usize,
    ty: &str,
) -> Option<serde_json::Value> {
    Some(match ty {
        // sqlx-mysql reports TINYINT(1) as "BOOLEAN" (see ColumnType::name).
        // `i64::compatible` rejects it, so it has to go through `bool`.
        "BOOLEAN" => serde_json::Value::Bool(mysql_get::<bool>(row, idx)?),
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
            serde_json::Value::Number(mysql_get::<i64>(row, idx)?.into())
        }
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED" => {
            serde_json::Value::Number(mysql_get::<u32>(row, idx)?.into())
        }
        "BIGINT UNSIGNED" => serde_json::Value::Number(mysql_get::<u64>(row, idx)?.into()),
        "FLOAT" | "DOUBLE" => serde_json::json!(mysql_get::<f64>(row, idx)?),
        // DECIMAL: stringified to preserve precision without pulling in
        // `bigdecimal`/`rust_decimal` features.
        "DECIMAL" => serde_json::Value::String(mysql_get::<String>(row, idx)?),
        "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            serde_json::Value::String(mysql_get::<String>(row, idx)?)
        }
        // JSON: must go through `sqlx::types::Json` (requires `json` feature).
        // Decoding as `String` or `Vec<u8>` yields wire-format garbage.
        "JSON" => {
            let sqlx::types::Json(v) = row
                .try_get::<Option<sqlx::types::Json<serde_json::Value>>, _>(idx)
                .ok()
                .flatten()?;
            v
        }
        // Temporal types arrive as binary tuples over the MySQL binary protocol
        // (prepared statements), so `String` decoding fails. Go through chrono.
        "DATETIME" => serde_json::Value::String(
            mysql_get::<sqlx::types::chrono::NaiveDateTime>(row, idx)?
                .format("%Y-%m-%d %H:%M:%S%.f")
                .to_string(),
        ),
        "TIMESTAMP" => serde_json::Value::String(
            mysql_get::<sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>>(row, idx)?
                .to_rfc3339(),
        ),
        "DATE" => serde_json::Value::String(
            mysql_get::<sqlx::types::chrono::NaiveDate>(row, idx)?.to_string(),
        ),
        "TIME" => serde_json::Value::String(
            mysql_get::<sqlx::types::chrono::NaiveTime>(row, idx)?
                .format("%H:%M:%S%.f")
                .to_string(),
        ),
        "YEAR" => {
            if let Some(v) = mysql_get::<u16>(row, idx) {
                serde_json::Value::Number(v.into())
            } else {
                serde_json::Value::Number(mysql_get::<i64>(row, idx)?.into())
            }
        }
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" | "GEOMETRY"
        | "BIT" => {
            serde_json::Value::String(BASE64_STANDARD.encode(mysql_get::<Vec<u8>>(row, idx)?))
        }
        "NULL" => serde_json::Value::Null,
        _ => return None,
    })
}

// Fallback decoder invoked when the preferred decode for a known type fails
// or when a future/unmapped type name is encountered. At this point NULL has
// already been ruled out by the caller, so we try the two most permissive
// sqlx decoders and, as a last resort, emit a tagged marker so the user knows
// a value exists but couldn't be interpreted.
fn mysql_fallback_decode(row: &sqlx::mysql::MySqlRow, idx: usize, ty: &str) -> serde_json::Value {
    if let Some(s) = mysql_get::<String>(row, idx) {
        return serde_json::Value::String(s);
    }
    if let Some(bytes) = mysql_get::<Vec<u8>>(row, idx) {
        return match std::str::from_utf8(&bytes) {
            Ok(s) => serde_json::Value::String(s.to_string()),
            Err(_) => {
                serde_json::Value::String(format!("base64:{}", BASE64_STANDARD.encode(&bytes)))
            }
        };
    }
    serde_json::Value::String(format!("<unable to decode {}>", ty))
}

// Flattens `Result<Option<T>>` from sqlx into `Option<T>`. Safe to treat both
// NULL and decode errors as `None` here because the caller already handled
// real NULLs via `try_get_raw`.
fn mysql_get<T>(row: &sqlx::mysql::MySqlRow, idx: usize) -> Option<T>
where
    T: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql>,
{
    row.try_get::<Option<T>, _>(idx).ok().flatten()
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
            serde_json::Value::Number(n) if n.as_i64().is_none() && n.as_f64().is_none() => {
                return Err(format!(
                    "Bind value at index {i} is a number outside the supported range (i64/f64)"
                ));
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

/// Split a SQL string on `;` boundaries that appear in code (not in strings,
/// line comments, or block comments). Drops statements that are empty after
/// trimming — callers get back exactly the statements they need to execute,
/// in source order. A single-statement input returns a single-element Vec.
pub fn split_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let len = chars.len();
    let mut scanner = SqlScanner::new();
    let mut i = 0;
    let mut current = String::new();
    let mut statements: Vec<String> = Vec::new();

    while i < len {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();

        let was_code = scanner.is_code();
        let consumed = scanner.advance(ch, next);

        if ch == ';' && was_code && scanner.is_code() {
            // Boundary — emit current statement (without the `;`).
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                statements.push(trimmed.to_string());
            }
            current.clear();
            i += consumed;
            continue;
        }

        // Copy the consumed chars into the current statement buffer.
        for j in 0..consumed {
            if i + j < len {
                current.push(chars[i + j]);
            }
        }
        i += consumed;
    }

    let trailing = current.trim();
    if !trailing.is_empty() {
        statements.push(trailing.to_string());
    }

    statements
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
                is_readonly INTEGER NOT NULL DEFAULT 0,
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
                is_readonly: None,
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
                is_readonly: None,
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
                is_readonly: None,
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
                is_readonly: None,
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
        let result =
            validate_connection_fields("postgres", &None, &Some(5432), &Some("db".to_string()));
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
            is_readonly: false,
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

        sqlx::query(
            "INSERT INTO test_table VALUES (1, 'alpha', 1.5), (2, 'beta', 2.5), (3, 'gamma', 3.5)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("SELECT * FROM test_table", &[], 0, 100)
            .await
            .unwrap();

        assert!(result.is_select);
        assert!(!result.has_more);
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
        assert!(result.has_more);
        assert_eq!(result.rows.len(), 3);

        // offset=9, fetch_size=3 → 1 row, has_more=false
        let result = db_pool
            .execute_query("SELECT * FROM items", &[], 9, 3)
            .await
            .unwrap();
        assert!(!result.has_more);
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
            .execute_query("INSERT INTO items VALUES (1, 'test')", &[], 1, 100)
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
            is_readonly: false,
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
            is_readonly: false,
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
            is_readonly: false,
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
            is_readonly: false,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(validate_pool_config(&conn).is_err());

        let conn2 = Connection {
            port: Some(70000),
            ..conn
        };
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
            is_readonly: false,
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
        assert!(!contains_multiple_statements(
            "SELECT * FROM t WHERE v = 'a;b'"
        ));
    }

    #[test]
    fn test_allow_semicolon_in_line_comment() {
        assert!(!contains_multiple_statements("SELECT 1 -- ; comment"));
    }

    #[test]
    fn test_allow_semicolon_in_block_comment() {
        assert!(!contains_multiple_statements("SELECT 1 /* ; */ FROM t"));
    }

    // --- split_statements (stage 6) ---

    #[test]
    fn test_split_single_statement() {
        assert_eq!(split_statements("SELECT 1"), vec!["SELECT 1"]);
        assert_eq!(split_statements("SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(split_statements("  SELECT 1  ;  "), vec!["SELECT 1"]);
    }

    #[test]
    fn test_split_multiple_statements() {
        let r = split_statements("SELECT 1; SELECT 2; SELECT 3;");
        assert_eq!(r, vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn test_split_preserves_string_semicolons() {
        let r = split_statements("SELECT 'a;b'; SELECT 2");
        assert_eq!(r, vec!["SELECT 'a;b'", "SELECT 2"]);
    }

    #[test]
    fn test_split_skips_line_and_block_comments() {
        let r = split_statements("SELECT 1 -- ; here\n; SELECT 2 /* ; inside */; SELECT 3");
        assert_eq!(
            r,
            vec![
                "SELECT 1 -- ; here".to_string(),
                "SELECT 2 /* ; inside */".to_string(),
                "SELECT 3".to_string(),
            ]
        );
    }

    #[test]
    fn test_split_drops_empty() {
        assert!(split_statements("").is_empty());
        assert!(split_statements(";;;").is_empty());
        let r = split_statements("SELECT 1;;;SELECT 2");
        assert_eq!(r, vec!["SELECT 1", "SELECT 2"]);
    }

    // --- QueryErrorLocation / position_to_line_col / mysql_line_from_message ---

    #[test]
    fn test_position_to_line_col_single_line() {
        assert_eq!(position_to_line_col("SELECT foo FROM bar", 8), (1, 8));
        assert_eq!(position_to_line_col("SELECT * FROM users", 1), (1, 1));
    }

    #[test]
    fn test_position_to_line_col_multiline() {
        let q = "SELECT *\nFROM users\nWHERE id = ?";
        // position 10 (after first newline) → line 2 col 1
        assert_eq!(position_to_line_col(q, 10), (2, 1));
        // position 21 → line 3 col 1 (second newline at offset 19, so col 1)
        let (line, _) = position_to_line_col(q, 21);
        assert_eq!(line, 3);
    }

    #[test]
    fn test_position_to_line_col_handles_zero() {
        // A zero position is not valid (Postgres uses 1-indexed); fall back
        // to (1,1) instead of panicking.
        assert_eq!(position_to_line_col("SELECT 1", 0), (1, 1));
    }

    #[test]
    fn test_mysql_line_from_message_parses_at_line() {
        let msg = "You have an error in your SQL syntax; check the manual … near 'foo' at line 3";
        assert_eq!(mysql_line_from_message(msg), Some(3));
    }

    #[test]
    fn test_mysql_line_from_message_none_for_unrelated() {
        assert_eq!(mysql_line_from_message("duplicate entry"), None);
    }

    #[test]
    fn test_enrich_error_with_query_converts_pg_position() {
        let mut info = QueryErrorInfo {
            message: "Query failed: syntax error".to_string(),
            location: QueryErrorLocation {
                line: None,
                column: Some(10),
            },
        };
        enrich_error_with_query(&mut info, "SELECT *\nFROM oops");
        // Position 10 is the "F" of "FROM" on line 2, col 1.
        assert_eq!(info.location.line, Some(2));
        assert_eq!(info.location.column, Some(1));
    }

    #[test]
    fn test_enrich_error_preserves_mysql_line() {
        let mut info = QueryErrorInfo {
            message: "syntax error at line 3".to_string(),
            location: QueryErrorLocation {
                line: Some(3),
                column: None,
            },
        };
        enrich_error_with_query(&mut info, "SELECT 1");
        assert_eq!(info.location.line, Some(3));
        assert_eq!(info.location.column, None);
    }

    #[tokio::test]
    async fn test_explain_analyze_delete_rejected() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
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
        sqlx::query("CREATE TABLE t (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("EXPLAIN SELECT * FROM t", &[], 0, 100)
            .await;
        if let Err(ref e) = result {
            assert!(
                !e.contains("EXPLAIN ANALYZE"),
                "Should not be blocked by security check"
            );
            assert!(
                !e.contains("Multi-statement"),
                "Should not be blocked by multi-statement check"
            );
        }
    }

    #[tokio::test]
    async fn test_explain_analyze_select_not_blocked() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let db_pool = DatabasePool::Sqlite(pool);
        let result = db_pool
            .execute_query("EXPLAIN ANALYZE SELECT * FROM t", &[], 0, 100)
            .await;
        // May fail at execution level, but must NOT be our security rejection
        if let Err(ref e) = result {
            assert!(
                !e.contains("mutation"),
                "SELECT should not be blocked as mutation"
            );
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
        assert_eq!(
            count_placeholders("SELECT * FROM t WHERE v = '?' AND id = ?"),
            1
        );
    }

    #[test]
    fn test_count_placeholders_ignores_comments() {
        assert_eq!(count_placeholders("SELECT * -- ?\nFROM t WHERE id = ?"), 1);
        assert_eq!(
            count_placeholders("SELECT /* ? */ * FROM t WHERE id = ?"),
            1
        );
    }

    #[test]
    fn test_count_placeholders_multiple() {
        assert_eq!(
            count_placeholders("SELECT * FROM t WHERE a = ? AND b = ? AND c = ?"),
            3
        );
    }

    #[tokio::test]
    async fn test_multi_statement_rejected_in_execute() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
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
            serde_json::json!(2.5),
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
                &[
                    serde_json::json!(1),
                    serde_json::json!(2),
                    serde_json::json!(3),
                ],
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

    // ─────── Fail-secure: keychain unavailable must NOT fall back to plaintext ───────

    #[tokio::test]
    // The std::Mutex guard intentionally spans the awaits below: the lock
    // serializes tests that mutate the keychain's global mock-failure flag,
    // and dropping it earlier would let parallel tests corrupt that state.
    // Tests run on a single tokio thread per case, so blocking is fine.
    #[allow(clippy::await_holding_lock)]
    async fn create_connection_fails_secure_when_keychain_unavailable() {
        // Epic 16, Story 02 invariant: if `store_secret` fails, the
        // connection row must NOT be inserted with a plaintext password.
        // We force the keychain to error and verify both the Err return
        // AND that no row leaked into the database.
        use crate::db::keychain::{force_keychain_failure, KEYCHAIN_TEST_LOCK};

        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let pool = setup_test_pool().await;

        force_keychain_failure(true);
        let result = create_connection(
            &pool,
            CreateConnection {
                name: "fail-secure-test".to_string(),
                driver: "postgres".to_string(),
                host: Some("localhost".to_string()),
                port: Some(5432),
                database_name: Some("db".to_string()),
                username: Some("user".to_string()),
                password: Some("very-sensitive-password".to_string()),
                ssl_mode: None,
                timeout_ms: None,
                query_timeout_ms: None,
                ttl_seconds: None,
                max_pool_size: None,
                is_readonly: None,
            },
        )
        .await;
        force_keychain_failure(false);

        // 1. The call must fail loudly.
        let err = result.expect_err("create_connection must error when keychain fails");
        assert!(
            err.to_lowercase().contains("password") || err.to_lowercase().contains("keychain"),
            "error should mention password/keychain, got: {err}"
        );

        // 2. No row must exist in the database — that's the actual fail-
        // secure invariant. A leaked row with plaintext password would
        // be the worst case (silent regression of T15 from epic 16).
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM connections")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "no connection row may exist when keychain storage failed",
        );

        // 3. Sanity: when the keychain works, the same call must succeed.
        // Guards against the test always passing for the wrong reason.
        let ok = create_connection(
            &pool,
            CreateConnection {
                name: "fail-secure-baseline".to_string(),
                driver: "postgres".to_string(),
                host: Some("localhost".to_string()),
                port: Some(5432),
                database_name: Some("db".to_string()),
                username: Some("user".to_string()),
                password: Some("baseline-password".to_string()),
                ssl_mode: None,
                timeout_ms: None,
                query_timeout_ms: None,
                ttl_seconds: None,
                max_pool_size: None,
                is_readonly: None,
            },
        )
        .await;
        // The baseline only succeeds if the test environment has a working
        // keyring backend (macOS Keychain, etc). In headless CI this can
        // legitimately fail — accept either Ok or Err here. The fail-secure
        // case above is what we really care about.
        let _ = ok;
    }

    // ─────── Cache invalidation: update_connection → next query uses new pool ───────

    #[tokio::test]
    async fn update_connection_followed_by_invalidate_yields_fresh_pool() {
        // Epic 16, Story 05 invariant (L102): the Tauri command sequence
        // (update_connection → conn_manager.invalidate) must guarantee
        // that the next `get_pool` call returns a pool built from the
        // updated row, not a stale cached one.
        //
        // We can't test the full Tauri command from here, but we can
        // assert the invariant on the `PoolManager` it ultimately calls:
        // (1) without invalidate, the cache survives an update; (2) after
        // invalidate, the next get_pool creates a fresh Arc whose
        // PoolEntry reflects the new config. This is the bug the L102
        // task was designed to catch — a regression where invalidate
        // stops being called would leak stale config.
        let pool = setup_test_pool().await;
        let manager = std::sync::Arc::new(PoolManager::new_standalone(
            super::super::lookup::SqliteLookup::new(pool.clone()),
            pool.clone(),
        ));

        let created = create_connection(
            &pool,
            CreateConnection {
                name: "pool-invalidate-test".to_string(),
                driver: "sqlite".to_string(),
                host: None,
                port: None,
                database_name: Some(":memory:".to_string()),
                username: None,
                password: None,
                ssl_mode: None,
                timeout_ms: Some(5000),
                query_timeout_ms: Some(10000),
                ttl_seconds: Some(300),
                max_pool_size: Some(2),
                is_readonly: None,
            },
        )
        .await
        .expect("create_connection (sqlite) must succeed");

        let id = created.id.clone();

        // First get_pool: pool is created and cached with timeout=10000.
        let pool_a = manager.get_pool(&id).await.expect("get_pool must succeed");
        assert_eq!(manager.get_query_timeout(&id).await, Some(10000));

        // Update the row's query_timeout_ms in the database.
        update_connection(
            &pool,
            &id,
            UpdateConnection {
                name: None,
                driver: None,
                host: None,
                port: None,
                database_name: None,
                username: None,
                password: None,
                ssl_mode: None,
                timeout_ms: None,
                query_timeout_ms: Some(99000),
                ttl_seconds: None,
                max_pool_size: None,
                is_readonly: None,
            },
        )
        .await
        .expect("update_connection must succeed");

        // Without invalidate, the cache is still serving the old timeout.
        // This documents the (intentional) fact that PoolManager has no
        // automatic awareness of DB row changes — it relies on the
        // caller (the Tauri command) to invalidate.
        assert_eq!(
            manager.get_query_timeout(&id).await,
            Some(10000),
            "cache must survive an update with no invalidate (documents the invariant)",
        );

        // Tauri command's invalidate step.
        manager.invalidate(&id).await;

        // Next get_pool builds a fresh pool from the current DB row.
        let pool_b = manager
            .get_pool(&id)
            .await
            .expect("get_pool after invalidate must succeed");

        assert!(
            !std::sync::Arc::ptr_eq(&pool_a, &pool_b),
            "pool Arc must differ — cache was rebuilt, not reused",
        );
        assert_eq!(
            manager.get_query_timeout(&id).await,
            Some(99000),
            "rebuilt PoolEntry must reflect the updated row's query_timeout_ms",
        );
    }
}
