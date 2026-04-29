//! File-backed connections store.
//!
//! Source of truth is `<vault_root>/connections.toml`. This module owns
//! reading, validating, mutating, and atomically writing it; secret
//! values flow through the OS keychain (ADR 0002), never to disk.
//!
//! The pool manager (`db::connections::PoolManager`) keeps using the
//! legacy `db::connections::Connection` struct; this module ships a
//! converter so the rest of the runtime is unchanged.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::db::connections::Connection as LegacyConnection;
use crate::db::keychain::{
    delete_secret, get_secret, resolve_secret_ref, store_secret, KEYCHAIN_SENTINEL,
};

use super::atomic::{read_toml, write_toml};
use super::connections::{
    CommonFields, Connection, ConnectionsFile, MysqlConfig, PostgresConfig, SqliteConfig,
};
use super::validate::validate_connections_file;
use super::Version;

const CONNECTIONS_FILE: &str = "connections.toml";

// --- input DTOs -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CreateConnectionInput {
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    /// Raw password from the UI. Always stored in the keychain; the
    /// TOML file holds only a `{{keychain:...}}` reference.
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
    pub is_readonly: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateConnectionInput {
    pub driver: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    /// `Some(empty)` clears; `Some(value)` rewrites; `None` keeps.
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
    pub is_readonly: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionPublic {
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database_name: Option<String>,
    pub username: Option<String>,
    pub has_password: bool,
    pub ssl_mode: Option<String>,
    pub is_readonly: bool,
    pub description: Option<String>,
}

// --- store ---------------------------------------------------------------

/// Cached parse + the on-disk mtime that produced it.
#[derive(Debug, Clone)]
struct Cached {
    mtime: Option<SystemTime>,
    file: ConnectionsFile,
}

/// File-backed CRUD over `connections.toml`.
pub struct ConnectionsStore {
    vault_root: PathBuf,
    cache: RwLock<Option<Cached>>,
}

impl ConnectionsStore {
    pub fn new(vault_root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            vault_root: vault_root.into(),
            cache: RwLock::new(None),
        })
    }

    fn path(&self) -> PathBuf {
        self.vault_root.join(CONNECTIONS_FILE)
    }

    fn current_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(self.path())
            .ok()
            .and_then(|m| m.modified().ok())
    }

    /// Returns a parsed `ConnectionsFile`, using the cache when the
    /// on-disk mtime hasn't changed. If the file doesn't exist, returns
    /// an empty (default) file.
    async fn load(&self) -> Result<ConnectionsFile, String> {
        let path = self.path();
        let disk_mtime = self.current_mtime();

        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.as_ref() {
                if cached.mtime == disk_mtime {
                    return Ok(cached.file.clone());
                }
            }
        }

        let file = if path.exists() {
            read_toml::<ConnectionsFile>(&path)
                .map_err(|e| format!("read connections.toml: {e}"))?
        } else {
            ConnectionsFile {
                version: Version::V1,
                connections: BTreeMap::new(),
            }
        };

        let mut cache = self.cache.write().await;
        *cache = Some(Cached {
            mtime: disk_mtime,
            file: file.clone(),
        });
        Ok(file)
    }

    /// Validates and persists `file`. Refuses to write when the
    /// validator returns hard errors.
    async fn persist(&self, file: ConnectionsFile) -> Result<(), String> {
        let report = validate_connections_file(&file);
        if report.has_errors() {
            let summary = report
                .issues
                .iter()
                .filter(|i| i.severity == super::validate::Severity::Error)
                .map(|i| format!("- {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "connections.toml refuses to save: validator found errors:\n{summary}"
            ));
        }
        let path = self.path();
        write_toml(&path, &file).map_err(|e| format!("write connections.toml: {e}"))?;

        let mut cache = self.cache.write().await;
        *cache = Some(Cached {
            mtime: self.current_mtime(),
            file,
        });
        Ok(())
    }

    /// Force the next read to hit disk. Called after external file
    /// changes (epic 11 file watcher).
    pub async fn invalidate_cache(&self) {
        let mut cache = self.cache.write().await;
        *cache = None;
    }

    pub async fn list_public(&self) -> Result<Vec<ConnectionPublic>, String> {
        let file = self.load().await?;
        Ok(file
            .connections
            .iter()
            .map(|(name, c)| to_public(name, c))
            .collect())
    }

    pub async fn get(&self, name: &str) -> Result<Option<ConnectionPublic>, String> {
        let file = self.load().await?;
        Ok(file.connections.get(name).map(|c| to_public(name, c)))
    }

    /// Returns the legacy `db::connections::Connection` shape that the
    /// pool manager already understands. Resolves password references
    /// against the keychain so the returned struct is usable for actual
    /// DB connection.
    pub async fn get_legacy(&self, name: &str) -> Result<Option<LegacyConnection>, String> {
        let file = self.load().await?;
        let Some(conn) = file.connections.get(name) else {
            return Ok(None);
        };
        Ok(Some(to_legacy(name, conn)?))
    }

    pub async fn create(&self, input: CreateConnectionInput) -> Result<ConnectionPublic, String> {
        let mut file = self.load().await?;
        if file.connections.contains_key(&input.name) {
            return Err(format!("connection '{}' already exists", input.name));
        }

        let conn = build_connection_from_input(
            &input.name,
            &input.driver,
            input.host.as_deref(),
            input.port,
            input.database_name.as_deref(),
            input.username.as_deref(),
            input.password.as_deref(),
            input.ssl_mode.as_deref(),
            input.is_readonly.unwrap_or(false),
            input.description.as_deref(),
        )?;

        file.connections.insert(input.name.clone(), conn.clone());
        self.persist(file).await?;
        Ok(to_public(&input.name, &conn))
    }

    pub async fn update(
        &self,
        name: &str,
        input: UpdateConnectionInput,
    ) -> Result<ConnectionPublic, String> {
        let mut file = self.load().await?;
        let existing = file
            .connections
            .get(name)
            .cloned()
            .ok_or_else(|| format!("connection '{name}' not found"))?;

        let driver_now = driver_string_for(&existing);
        let driver = input.driver.as_deref().unwrap_or(driver_now);
        let host = input.host.or_else(|| host_of(&existing));
        let port = input.port.or_else(|| port_of(&existing));
        let database_name = input.database_name.or_else(|| database_name_of(&existing));
        let username = input.username.or_else(|| username_of(&existing));
        let ssl_mode = input.ssl_mode.or_else(|| ssl_mode_of(&existing));
        let is_readonly = input.is_readonly.unwrap_or(existing_readonly(&existing));
        let description = input.description.or_else(|| description_of(&existing));

        // Password handling:
        //   None        → keep existing
        //   Some("")    → clear (delete keychain entry, drop ref)
        //   Some(raw)   → write fresh keychain entry, write fresh ref
        let password_to_pass = match input.password {
            None => carry_password_ref(&existing),
            Some(empty) if empty.is_empty() => {
                let _ = delete_secret(&conn_password_keychain_key(name));
                None
            }
            Some(new_pw) => Some(new_pw),
        };

        let conn = build_connection_from_input(
            name,
            driver,
            host.as_deref(),
            port,
            database_name.as_deref(),
            username.as_deref(),
            password_to_pass.as_deref(),
            ssl_mode.as_deref(),
            is_readonly,
            description.as_deref(),
        )?;

        file.connections.insert(name.to_string(), conn.clone());
        self.persist(file).await?;
        Ok(to_public(name, &conn))
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let mut file = self.load().await?;
        if file.connections.remove(name).is_none() {
            return Err(format!("connection '{name}' not found"));
        };
        self.persist(file).await?;
        let _ = delete_secret(&conn_password_keychain_key(name));
        Ok(())
    }
}

// --- conversion helpers ---------------------------------------------------

fn driver_string_for(c: &Connection) -> &'static str {
    match c {
        Connection::Postgres(_) => "postgres",
        Connection::Mysql(_) => "mysql",
        Connection::Sqlite(_) => "sqlite",
        Connection::Mongo(_) => "mongo",
        Connection::Http(_) => "http",
        Connection::Ws(_) => "ws",
        Connection::Grpc(_) => "grpc",
        Connection::Graphql(_) => "graphql",
        Connection::Bigquery(_) => "bigquery",
        Connection::Shell(_) => "shell",
    }
}

fn host_of(c: &Connection) -> Option<String> {
    match c {
        Connection::Postgres(p) => Some(p.host.clone()),
        Connection::Mysql(m) => Some(m.host.clone()),
        _ => None,
    }
}

fn port_of(c: &Connection) -> Option<u16> {
    match c {
        Connection::Postgres(p) => Some(p.port),
        Connection::Mysql(m) => Some(m.port),
        _ => None,
    }
}

fn database_name_of(c: &Connection) -> Option<String> {
    match c {
        Connection::Postgres(p) => Some(p.database.clone()),
        Connection::Mysql(m) => Some(m.database.clone()),
        Connection::Sqlite(s) => Some(s.path.clone()),
        _ => None,
    }
}

fn username_of(c: &Connection) -> Option<String> {
    match c {
        Connection::Postgres(p) => Some(p.user.clone()),
        Connection::Mysql(m) => Some(m.user.clone()),
        _ => None,
    }
}

fn ssl_mode_of(c: &Connection) -> Option<String> {
    match c {
        Connection::Postgres(p) => p.ssl_mode.clone(),
        _ => None,
    }
}

fn existing_readonly(c: &Connection) -> bool {
    common_of(c).read_only
}

fn description_of(c: &Connection) -> Option<String> {
    common_of(c).description.clone()
}

fn common_of(c: &Connection) -> &CommonFields {
    match c {
        Connection::Postgres(p) => &p.common,
        Connection::Mysql(m) => &m.common,
        Connection::Sqlite(s) => &s.common,
        Connection::Mongo(m) => &m.common,
        Connection::Http(h) => &h.common,
        Connection::Ws(w) => &w.common,
        Connection::Grpc(g) => &g.common,
        Connection::Graphql(g) => &g.common,
        Connection::Bigquery(b) => &b.common,
        Connection::Shell(s) => &s.common,
    }
}

fn carry_password_ref(c: &Connection) -> Option<String> {
    // For variants that carry a password, an "unchanged" update keeps
    // the existing reference verbatim — `build_connection_from_input`
    // re-saves it back into the variant.
    match c {
        Connection::Postgres(p) => Some(p.password.clone()),
        Connection::Mysql(m) => Some(m.password.clone()),
        _ => None,
    }
}

/// Build a `Connection` from input. If `password` is provided AND it
/// isn't already a `{{...}}` reference, it gets stored in the keychain
/// and the variant ends up with the matching reference string.
#[allow(clippy::too_many_arguments)]
fn build_connection_from_input(
    name: &str,
    driver: &str,
    host: Option<&str>,
    port: Option<u16>,
    database_name: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
    ssl_mode: Option<&str>,
    is_readonly: bool,
    description: Option<&str>,
) -> Result<Connection, String> {
    let common = CommonFields {
        description: description.map(String::from),
        read_only: is_readonly,
    };

    match driver {
        "postgres" => {
            let host = require(host, "host", driver)?;
            let database = require(database_name, "database_name", driver)?;
            let user = require(username, "username", driver)?;
            let password_ref = ensure_password_ref(name, password)?;
            Ok(Connection::Postgres(PostgresConfig {
                host: host.to_string(),
                port: port.unwrap_or(5432),
                database: database.to_string(),
                user: user.to_string(),
                password: password_ref,
                ssl_mode: ssl_mode.map(String::from),
                common,
            }))
        }
        "mysql" => {
            let host = require(host, "host", driver)?;
            let database = require(database_name, "database_name", driver)?;
            let user = require(username, "username", driver)?;
            let password_ref = ensure_password_ref(name, password)?;
            Ok(Connection::Mysql(MysqlConfig {
                host: host.to_string(),
                port: port.unwrap_or(3306),
                database: database.to_string(),
                user: user.to_string(),
                password: password_ref,
                common,
            }))
        }
        "sqlite" => {
            let path = require(database_name, "database_name (sqlite path)", driver)?;
            Ok(Connection::Sqlite(SqliteConfig {
                path: path.to_string(),
                common,
            }))
        }
        // Variants below are reachable from a hand-edited TOML but the
        // CRUD UI in v1 only creates the three DB types above. Reject
        // create/update for the others until the UI catches up.
        "mongo" | "http" | "ws" | "grpc" | "graphql" | "bigquery" | "shell" => {
            Err(reject_unimplemented(driver))
        }
        other => Err(format!("unsupported driver: {other}")),
    }
}

/// Convert `password` input to a `{{keychain:...}}` reference. If the
/// caller already gave us a reference, we keep it. Otherwise we store
/// the raw value in the keychain and return the matching reference.
fn ensure_password_ref(name: &str, password: Option<&str>) -> Result<String, String> {
    let Some(raw) = password else {
        return Ok(String::new());
    };
    if raw.is_empty() {
        return Ok(String::new());
    }
    if super::validate::is_secret_ref(raw) {
        return Ok(raw.to_string());
    }
    let key = conn_password_keychain_key(name);
    store_secret(&key, raw).map_err(|e| format!("Failed to store password securely: {e}"))?;
    Ok(format_password_ref(name))
}

/// Keychain key for a connection's password. Mirrors the structure of
/// the reference syntax, joined with `:`.
pub fn conn_password_keychain_key(name: &str) -> String {
    format!("conn:{name}:password")
}

fn format_password_ref(name: &str) -> String {
    format!("{{{{keychain:conn:{name}:password}}}}")
}

fn require<'a>(value: Option<&'a str>, field: &str, driver: &str) -> Result<&'a str, String> {
    match value {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("{field} is required for {driver}")),
    }
}

fn reject_unimplemented(driver: &str) -> String {
    format!(
        "creating/updating `{driver}` connections from the app UI is not supported yet; \
         hand-edit connections.toml if you need this variant"
    )
}

fn to_public(name: &str, c: &Connection) -> ConnectionPublic {
    let driver = driver_string_for(c).to_string();
    let host = host_of(c);
    let port = port_of(c);
    let database_name = database_name_of(c);
    let username = username_of(c);
    let ssl_mode = ssl_mode_of(c);
    let common = common_of(c);
    let has_password = password_present(c);

    ConnectionPublic {
        name: name.to_string(),
        driver,
        host,
        port,
        database_name,
        username,
        has_password,
        ssl_mode,
        is_readonly: common.read_only,
        description: common.description.clone(),
    }
}

fn password_present(c: &Connection) -> bool {
    let password = match c {
        Connection::Postgres(p) => &p.password,
        Connection::Mysql(m) => &m.password,
        _ => return false,
    };
    if password.is_empty() {
        return false;
    }
    if super::validate::is_secret_ref(password) {
        // A reference counts as "has password" only when the keychain
        // actually has the entry. Best-effort lookup; a transient
        // keychain failure surfaces as has_password = false (caller
        // can then re-prompt).
        if let Some((_, address)) = parse_keychain_ref(password) {
            return get_secret(&address).map(|v| v.is_some()).unwrap_or(false);
        }
        false
    } else {
        // Legacy plaintext value (pre-migration) or sentinel.
        password != KEYCHAIN_SENTINEL
    }
}

/// Parse `{{keychain:NS:KEY}}` into `(backend, "NS:KEY")` for keychain
/// lookups. Returns `None` for non-keychain backends.
fn parse_keychain_ref(s: &str) -> Option<(&'static str, String)> {
    let trimmed = s.trim();
    if !(trimmed.starts_with("{{") && trimmed.ends_with("}}")) {
        return None;
    }
    let inner = &trimmed[2..trimmed.len() - 2];
    let (backend, address) = inner.split_once(':')?;
    if backend != "keychain" {
        return None;
    }
    Some(("keychain", address.to_string()))
}

/// Convert a vault-config Connection to the legacy struct understood by
/// the pool manager. Resolves password references via the keychain.
fn to_legacy(name: &str, c: &Connection) -> Result<LegacyConnection, String> {
    let driver = driver_string_for(c).to_string();
    let host = host_of(c);
    let port = port_of(c).map(|p| p as i64);
    let database_name = database_name_of(c);
    let username = username_of(c);
    let ssl_mode = ssl_mode_of(c);
    let is_readonly = common_of(c).read_only;

    let password = match c {
        Connection::Postgres(p) => resolve_password_ref(&p.password)?,
        Connection::Mysql(m) => resolve_password_ref(&m.password)?,
        _ => None,
    };

    Ok(LegacyConnection {
        id: name.to_string(),
        name: name.to_string(),
        driver,
        host,
        port,
        database_name,
        username,
        password,
        ssl_mode,
        timeout_ms: 10000,
        query_timeout_ms: 30000,
        ttl_seconds: 300,
        max_pool_size: 5,
        is_readonly,
        last_tested_at: None,
        created_at: String::new(),
        updated_at: String::new(),
    })
}

fn resolve_password_ref(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    if !super::validate::is_secret_ref(value) {
        // Plaintext (legacy) — pass through.
        return Ok(Some(value.to_string()));
    }
    match resolve_secret_ref(value) {
        Ok(Some(v)) => Ok(Some(v)),
        Ok(None) => Err(format!("secret reference {value} did not resolve")),
        Err(e) => Err(format!("keychain error resolving {value}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::keychain::KEYCHAIN_TEST_LOCK;
    use tempfile::TempDir;

    fn fresh_store() -> (Arc<ConnectionsStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = ConnectionsStore::new(tmp.path());
        (store, tmp)
    }

    fn unique_name(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[tokio::test]
    async fn list_on_empty_vault_returns_empty() {
        let (store, _t) = fresh_store();
        assert!(store.list_public().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_postgres_writes_file_and_keychain() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, t) = fresh_store();
        let name = unique_name("pg-create");
        let pub_ = store
            .create(CreateConnectionInput {
                name: name.clone(),
                driver: "postgres".into(),
                host: Some("localhost".into()),
                port: Some(5432),
                database_name: Some("test".into()),
                username: Some("u".into()),
                password: Some("hunter2".into()),
                ssl_mode: Some("require".into()),
                is_readonly: Some(false),
                description: None,
            })
            .await
            .expect("create");
        assert_eq!(pub_.name, name);
        assert_eq!(pub_.driver, "postgres");
        assert!(pub_.has_password);

        // File on disk has only a reference, never the raw password.
        let raw = std::fs::read_to_string(t.path().join("connections.toml")).unwrap();
        assert!(!raw.contains("hunter2"));
        assert!(raw.contains(&format!("{{{{keychain:conn:{name}:password}}}}")));

        // Cleanup keychain entry to keep the test environment clean.
        let _ = delete_secret(&conn_password_keychain_key(&name));
    }

    #[tokio::test]
    async fn create_rejects_duplicate_name() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, _t) = fresh_store();
        let name = unique_name("dup");

        let mk = |n: String| CreateConnectionInput {
            name: n,
            driver: "sqlite".into(),
            host: None,
            port: None,
            database_name: Some("/tmp/x.sqlite".into()),
            username: None,
            password: None,
            ssl_mode: None,
            is_readonly: None,
            description: None,
        };
        store.create(mk(name.clone())).await.unwrap();
        let err = store.create(mk(name)).await.unwrap_err();
        assert!(err.contains("already exists"));
    }

    #[tokio::test]
    async fn update_changes_only_provided_fields() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, _t) = fresh_store();
        let name = unique_name("up");

        store
            .create(CreateConnectionInput {
                name: name.clone(),
                driver: "postgres".into(),
                host: Some("h1".into()),
                port: Some(5432),
                database_name: Some("d".into()),
                username: Some("u".into()),
                password: Some("pw".into()),
                ssl_mode: None,
                is_readonly: Some(false),
                description: None,
            })
            .await
            .unwrap();

        // Update only host
        let updated = store
            .update(
                &name,
                UpdateConnectionInput {
                    host: Some("h2".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.host.as_deref(), Some("h2"));
        assert!(updated.has_password); // password preserved

        let _ = delete_secret(&conn_password_keychain_key(&name));
    }

    #[tokio::test]
    async fn update_password_some_empty_clears_it() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, t) = fresh_store();
        let name = unique_name("clear");

        store
            .create(CreateConnectionInput {
                name: name.clone(),
                driver: "postgres".into(),
                host: Some("h".into()),
                port: Some(5432),
                database_name: Some("d".into()),
                username: Some("u".into()),
                password: Some("pw".into()),
                ssl_mode: None,
                is_readonly: None,
                description: None,
            })
            .await
            .unwrap();

        store
            .update(
                &name,
                UpdateConnectionInput {
                    password: Some(String::new()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let pub_ = store.get(&name).await.unwrap().unwrap();
        assert!(!pub_.has_password);

        let raw = std::fs::read_to_string(t.path().join("connections.toml")).unwrap();
        assert!(!raw.contains("pw"));
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, _t) = fresh_store();
        let name = unique_name("del");

        store
            .create(CreateConnectionInput {
                name: name.clone(),
                driver: "sqlite".into(),
                host: None,
                port: None,
                database_name: Some("/tmp/d.sqlite".into()),
                username: None,
                password: None,
                ssl_mode: None,
                is_readonly: None,
                description: None,
            })
            .await
            .unwrap();
        store.delete(&name).await.unwrap();
        assert!(store.get(&name).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cache_hits_when_mtime_unchanged() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, _t) = fresh_store();
        let name = unique_name("cache");

        store
            .create(CreateConnectionInput {
                name: name.clone(),
                driver: "sqlite".into(),
                host: None,
                port: None,
                database_name: Some("/tmp/c.sqlite".into()),
                username: None,
                password: None,
                ssl_mode: None,
                is_readonly: None,
                description: None,
            })
            .await
            .unwrap();

        // List twice; second should not re-parse (we can't directly observe
        // that, so just sanity-check the result is consistent).
        let a = store.list_public().await.unwrap();
        let b = store.list_public().await.unwrap();
        assert_eq!(a.len(), b.len());
    }

    #[tokio::test]
    async fn unsupported_driver_for_create_errors() {
        let (store, _t) = fresh_store();
        let err = store
            .create(CreateConnectionInput {
                name: "x".into(),
                driver: "unknown".into(),
                host: None,
                port: None,
                database_name: None,
                username: None,
                password: None,
                ssl_mode: None,
                is_readonly: None,
                description: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("unsupported driver"));
    }
}
