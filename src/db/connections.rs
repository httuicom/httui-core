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
        // Check cache
        {
            let pools = self.pools.read().await;
            if let Some(entry) = pools.get(connection_id) {
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

fn build_connection_string(conn: &Connection) -> Result<String, String> {
    use super::keychain::{conn_password_key, resolve_value, KEYCHAIN_SENTINEL};

    match conn.driver.as_str() {
        "postgres" => {
            let host = conn.host.as_deref().unwrap_or("localhost");
            let port = conn.port.unwrap_or(5432);
            let db = conn
                .database_name
                .as_deref()
                .ok_or("database_name is required for postgres")?;
            let user = conn.username.as_deref().unwrap_or("postgres");
            let db_password = conn.password.as_deref().unwrap_or("");
            let password = if db_password == KEYCHAIN_SENTINEL {
                resolve_value(db_password, &conn_password_key(&conn.id)).unwrap_or_default()
            } else {
                db_password.to_string()
            };
            let ssl = conn.ssl_mode.as_deref().unwrap_or("disable");
            Ok(format!(
                "postgres://{user}:{password}@{host}:{port}/{db}?sslmode={ssl}"
            ))
        }
        "mysql" => {
            // MySQL uses the typed MySqlConnectOptions builder in create_pool;
            // build_connection_string should not be called for mysql.
            Err("build_connection_string does not support mysql; use build_mysql_connect_options".to_string())
        }
        "sqlite" => {
            let path = conn
                .database_name
                .as_deref()
                .ok_or("database_name (file path) is required for sqlite")?;
            Ok(format!("sqlite:{path}"))
        }
        other => Err(format!("Unsupported driver: {other}")),
    }
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
    let ssl_mode = match conn.ssl_mode.as_deref().unwrap_or("disable") {
        "require" | "verify-ca" | "verify-full" => MySqlSslMode::Required,
        _ => MySqlSslMode::Disabled,
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

async fn create_pool(conn: &Connection) -> Result<DatabasePool, String> {
    let max_conns = conn.max_pool_size as u32;
    let timeout = Duration::from_millis(conn.timeout_ms as u64);

    match conn.driver.as_str() {
        "postgres" => {
            let url = build_connection_string(conn)?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(max_conns)
                .acquire_timeout(timeout)
                .connect(&url)
                .await
                .map_err(|e| format!("Failed to connect to postgres: {e}"))?;
            Ok(DatabasePool::Postgres(pool))
        }
        "mysql" => {
            let opts = build_mysql_connect_options(conn)?;
            let db_name = conn.database_name.clone().unwrap_or_default();
            let mut pool_opts = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(max_conns)
                .acquire_timeout(timeout);
            if !db_name.is_empty() {
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
                .map_err(|e| format!("Failed to connect to mysql: {e}"))?;
            Ok(DatabasePool::MySql(pool))
        }
        "sqlite" => {
            let url = build_connection_string(conn)?;
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(max_conns)
                .acquire_timeout(timeout)
                .connect(&url)
                .await
                .map_err(|e| format!("Failed to connect to sqlite: {e}"))?;
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
    let ssl_mode = input.ssl_mode.unwrap_or_else(|| "disable".to_string());
    let timeout_ms = input.timeout_ms.unwrap_or(10000);
    let query_timeout_ms = input.query_timeout_ms.unwrap_or(30000);
    let ttl_seconds = input.ttl_seconds.unwrap_or(300);
    let max_pool_size = input.max_pool_size.unwrap_or(5);

    // Store password in keychain if available, fallback to plaintext
    let db_password = if let Some(ref pw) = input.password {
        if !pw.is_empty() {
            use super::keychain::{conn_password_key, store_secret, KEYCHAIN_SENTINEL};
            match store_secret(&conn_password_key(&id), pw) {
                Ok(()) => Some(KEYCHAIN_SENTINEL.to_string()),
                Err(_) => input.password.clone(), // fallback to plaintext
            }
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
    // If a new password is provided, store in keychain
    let password = if let Some(ref new_pw) = input.password {
        if !new_pw.is_empty() {
            use super::keychain::{conn_password_key, store_secret, KEYCHAIN_SENTINEL};
            match store_secret(&conn_password_key(id), new_pw) {
                Ok(()) => Some(KEYCHAIN_SENTINEL.to_string()),
                Err(_) => Some(new_pw.clone()), // fallback to plaintext
            }
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
        }
        other => return Err(format!("Unsupported driver: {other}")),
    }
    Ok(())
}

// --- Query execution helpers (used by DbExecutor in executor/db/) ---

/// Row data as JSON-compatible values.
pub type JsonRow = Vec<serde_json::Value>;

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
        let trimmed = sql.trim_start().to_uppercase();
        let is_select = trimmed.starts_with("SELECT")
            || trimmed.starts_with("WITH")
            || trimmed.starts_with("SHOW")
            || trimmed.starts_with("DESCRIBE")
            || trimmed.starts_with("EXPLAIN")
            || trimmed.starts_with("PRAGMA");

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
        .map_err(|e| format!("Query failed: {e}"))?;

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
        .map_err(|e| format!("Query failed: {e}"))?;

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
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        other => query.bind(other.to_string()),
    }
}

fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> JsonRow {
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
        .map_err(|e| format!("Query failed: {e}"))?;

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
        .map_err(|e| format!("Query failed: {e}"))?;

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
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        other => query.bind(other.to_string()),
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
        .map_err(|e| format!("Query failed: {e}"))?;

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
        .map_err(|e| format!("Query failed: {e}"))?;

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
    match val {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        serde_json::Value::String(s) => query.bind(s.as_str()),
        other => query.bind(other.to_string()),
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

// --- Placeholder normalization ---

/// Convert `?` placeholders to `$N` for Postgres.
pub fn normalize_placeholders_to_pg(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut counter = 0u32;
    let mut in_string = false;
    let mut escape_next = false;

    for ch in sql.chars() {
        if escape_next {
            result.push(ch);
            escape_next = false;
            continue;
        }
        if ch == '\\' {
            result.push(ch);
            escape_next = true;
            continue;
        }
        if ch == '\'' {
            in_string = !in_string;
            result.push(ch);
            continue;
        }
        if ch == '?' && !in_string {
            counter += 1;
            result.push('$');
            result.push_str(&counter.to_string());
        } else {
            result.push(ch);
        }
    }

    result
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
    fn test_build_connection_string_postgres() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "postgres".to_string(),
            host: Some("db.example.com".to_string()),
            port: Some(5432),
            database_name: Some("mydb".to_string()),
            username: Some("admin".to_string()),
            password: Some("secret".to_string()),
            ssl_mode: Some("require".to_string()),
            timeout_ms: 10000,
            query_timeout_ms: 30000,
            ttl_seconds: 300,
            max_pool_size: 5,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };

        let url = build_connection_string(&conn).unwrap();
        assert_eq!(url, "postgres://admin:secret@db.example.com:5432/mydb?sslmode=require");
    }

    #[test]
    fn test_build_connection_string_sqlite() {
        let conn = Connection {
            id: "1".to_string(),
            name: "test".to_string(),
            driver: "sqlite".to_string(),
            host: None,
            port: None,
            database_name: Some("/tmp/test.db".to_string()),
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

        let url = build_connection_string(&conn).unwrap();
        assert_eq!(url, "sqlite:/tmp/test.db");
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
}
