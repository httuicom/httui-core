//! One-shot migration from MVP SQLite-backed storage to the v1 file
//! layout (ADR 0001). Reads from a live `notes.db` pool, writes
//! `connections.toml` and `envs/<name>.toml` via the new stores, and
//! optionally backs up the database first.
//!
//! Scope: connections + environments + variables. The `app_config`
//! prefs migration (theme/font/etc.) lives in Epic 19 alongside the
//! `UiPrefs` schema bump and the frontend cutover. See audit-005.
//!
//! The migration is **idempotent**: rerunning on an already-populated
//! vault is safe — duplicate-name failures from the underlying stores
//! are folded into a "skipped" counter rather than aborting.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use super::connections_store::CreateConnectionInput;
use super::environments_store::SetVarInput;
use super::{ConnectionsStore, EnvironmentsStore};
use crate::db::{connections, environments};

/// Per-call options. Build via [`MigrationOptions::default`] and
/// override what you need.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    /// Don't write anything; only walk the SQLite tables and report
    /// what would be migrated.
    pub dry_run: bool,
    /// Copy `notes.db` to `notes.db.pre-v1-backup` before any write.
    /// No-op when the file is missing or `dry_run` is true.
    pub backup: bool,
    /// Path of the per-machine `user.toml`. Currently only used to
    /// satisfy `EnvironmentsStore::new`; the prefs migration that
    /// would actually touch this file is deferred to Epic 19.
    pub user_config_path: PathBuf,
}

impl MigrationOptions {
    pub fn new(user_config_path: PathBuf) -> Self {
        Self {
            dry_run: false,
            backup: true,
            user_config_path,
        }
    }
}

/// Summary returned from [`run_migration`]. Counts reflect what was
/// actually written (or what would be, on a dry run).
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct MigrationReport {
    pub vault_path: String,
    pub backup_path: Option<String>,
    pub connections_migrated: usize,
    pub connections_skipped: usize,
    pub environments_migrated: usize,
    pub environments_skipped: usize,
    pub variables_migrated: usize,
    pub variables_skipped: usize,
    pub dry_run: bool,
    /// Free-form notes about deferred work / dual-storage warning.
    pub notes: Vec<String>,
}

/// Run the migration. See module docs for the contract.
pub async fn run_migration(
    pool: &SqlitePool,
    vault_path: &Path,
    opts: &MigrationOptions,
) -> Result<MigrationReport, String> {
    let mut report = MigrationReport {
        vault_path: vault_path.display().to_string(),
        dry_run: opts.dry_run,
        ..Default::default()
    };

    // Backup first — small file, cheap, undoable.
    if opts.backup && !opts.dry_run {
        let db_path = vault_path.join("notes.db");
        if db_path.exists() {
            let backup = vault_path.join("notes.db.pre-v1-backup");
            std::fs::copy(&db_path, &backup).map_err(|e| format!("backup notes.db: {e}"))?;
            report.backup_path = Some(backup.display().to_string());
        }
    }

    migrate_connections(pool, vault_path, opts.dry_run, &mut report).await?;
    migrate_environments(
        pool,
        vault_path,
        &opts.user_config_path,
        opts.dry_run,
        &mut report,
    )
    .await?;

    if opts.dry_run {
        report
            .notes
            .push("Dry run — nothing written. Re-run with dry_run=false to apply.".to_string());
    } else {
        report.notes.push(
            "Connections + envs migrated to TOML. The legacy SQLite tables are still read by the running app — Epic 19 cuts the Tauri commands over and drops the tables."
                .to_string(),
        );
    }

    Ok(report)
}

async fn migrate_connections(
    pool: &SqlitePool,
    vault_path: &Path,
    dry_run: bool,
    report: &mut MigrationReport,
) -> Result<(), String> {
    let conns = connections::list_connections(pool).await?;
    if dry_run {
        report.connections_migrated = conns.len();
        return Ok(());
    }

    let store: Arc<ConnectionsStore> = ConnectionsStore::new(vault_path);
    for c in conns {
        let input = legacy_to_input(&c);
        match store.create(input).await {
            Ok(_) => report.connections_migrated += 1,
            Err(e) if e.contains("already exists") => report.connections_skipped += 1,
            Err(e) => {
                return Err(format!("migrate connection '{}': {e}", c.name));
            }
        }
    }
    Ok(())
}

async fn migrate_environments(
    pool: &SqlitePool,
    vault_path: &Path,
    user_config_path: &Path,
    dry_run: bool,
    report: &mut MigrationReport,
) -> Result<(), String> {
    let envs = environments::list_environments(pool).await?;
    if dry_run {
        report.environments_migrated = envs.len();
        for env in &envs {
            let vars = environments::list_env_variables(pool, &env.id).await?;
            report.variables_migrated += vars.len();
        }
        return Ok(());
    }

    let store: Arc<EnvironmentsStore> = EnvironmentsStore::new(vault_path, user_config_path);
    for env in envs {
        match store.create_env(&env.name).await {
            Ok(_) => report.environments_migrated += 1,
            Err(e) if e.contains("already exists") => {
                report.environments_skipped += 1;
            }
            Err(e) => {
                return Err(format!("migrate env '{}': {e}", env.name));
            }
        }

        let vars = environments::list_env_variables(pool, &env.id).await?;
        for var in vars {
            let input = SetVarInput {
                env_name: env.name.clone(),
                key: var.key.clone(),
                value: var.value,
                is_secret: var.is_secret,
            };
            match store.set_var(input).await {
                Ok(_) => report.variables_migrated += 1,
                Err(e) if e.contains("already exists") => {
                    report.variables_skipped += 1;
                }
                Err(e) => {
                    return Err(format!("migrate var '{}/{}': {e}", env.name, var.key));
                }
            }
        }
    }
    Ok(())
}

fn legacy_to_input(c: &connections::Connection) -> CreateConnectionInput {
    CreateConnectionInput {
        name: c.name.clone(),
        driver: c.driver.clone(),
        host: c.host.clone(),
        port: c.port.and_then(|p| u16::try_from(p).ok()),
        database_name: c.database_name.clone(),
        username: c.username.clone(),
        password: c.password.clone(),
        ssl_mode: c.ssl_mode.clone(),
        is_readonly: Some(c.is_readonly),
        description: None,
    }
}

#[cfg(test)]
mod tests {
    // Tests use `KEYCHAIN_TEST_LOCK` (a `std::sync::Mutex`) and hold
    // it across the migration's `.await` boundaries. The lock is the
    // serialization point for the in-process keychain stub; safe to
    // hold here because `cargo test` runs each test in its own thread
    // and the lock is contention-free in practice.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::db::init_db;
    use tempfile::TempDir;

    /// Setup helper: fresh SQLite + populated with one connection,
    /// one environment, two vars (one secret).
    async fn populated_db(tmp: &TempDir) -> (SqlitePool, String) {
        let pool = init_db(tmp.path()).await.unwrap();

        // One connection
        connections::create_connection(
            &pool,
            connections::CreateConnection {
                name: "pg-staging".into(),
                driver: "postgres".into(),
                host: Some("pg.example.com".into()),
                port: Some(5432),
                database_name: Some("payments".into()),
                username: Some("app".into()),
                password: Some(String::new()),
                ssl_mode: Some("require".into()),
                timeout_ms: None,
                query_timeout_ms: None,
                ttl_seconds: None,
                max_pool_size: None,
                is_readonly: Some(false),
            },
        )
        .await
        .unwrap();

        // One environment + two vars (one plaintext, one secret)
        let env = environments::create_environment(&pool, "staging".into())
            .await
            .unwrap();
        environments::set_env_variable(
            &pool,
            &env.id,
            "BASE_URL".into(),
            "https://api.example.com".into(),
            false,
        )
        .await
        .unwrap();
        let env_id = env.id.clone();
        (pool, env_id)
    }

    #[tokio::test]
    async fn dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let (pool, _eid) = populated_db(&tmp).await;

        let opts = MigrationOptions {
            dry_run: true,
            backup: false,
            user_config_path: tmp.path().join("user.toml"),
        };
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let report = run_migration(&pool, &vault, &opts).await.unwrap();

        assert!(report.dry_run);
        assert_eq!(report.connections_migrated, 1);
        assert_eq!(report.environments_migrated, 1);
        assert_eq!(report.variables_migrated, 1);
        // Nothing on disk.
        assert!(!vault.join("connections.toml").exists());
        assert!(!vault.join("envs").exists());
    }

    #[tokio::test]
    async fn writes_files_in_normal_run() {
        let _g = crate::db::keychain::KEYCHAIN_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let (pool, _eid) = populated_db(&tmp).await;
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();

        let opts = MigrationOptions {
            dry_run: false,
            backup: false,
            user_config_path: tmp.path().join("user.toml"),
        };
        let report = run_migration(&pool, &vault, &opts).await.unwrap();

        assert_eq!(report.connections_migrated, 1);
        assert_eq!(report.environments_migrated, 1);
        assert_eq!(report.variables_migrated, 1);
        assert!(vault.join("connections.toml").exists());
        assert!(vault.join("envs/staging.toml").exists());
    }

    #[tokio::test]
    async fn backup_copies_db_when_present() {
        let _g = crate::db::keychain::KEYCHAIN_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let (pool, _eid) = populated_db(&tmp).await;
        // The init_db pool created `notes.db` under tmp.path(). Use
        // tmp.path() as the vault root for this test.
        let vault = tmp.path();
        let db_file = vault.join("notes.db");
        assert!(db_file.exists(), "notes.db should exist after init_db");

        let opts = MigrationOptions {
            dry_run: false,
            backup: true,
            user_config_path: tmp.path().join("user.toml"),
        };
        let report = run_migration(&pool, vault, &opts).await.unwrap();
        assert!(report.backup_path.is_some());
        assert!(vault.join("notes.db.pre-v1-backup").exists());
    }

    #[tokio::test]
    async fn rerun_is_idempotent() {
        let _g = crate::db::keychain::KEYCHAIN_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let (pool, _eid) = populated_db(&tmp).await;
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();

        let opts = MigrationOptions {
            dry_run: false,
            backup: false,
            user_config_path: tmp.path().join("user.toml"),
        };
        run_migration(&pool, &vault, &opts).await.unwrap();
        // Second run: everything already there — counts as skipped.
        let r2 = run_migration(&pool, &vault, &opts).await.unwrap();
        assert_eq!(r2.connections_skipped, 1);
        assert_eq!(r2.environments_skipped, 1);
    }

    #[tokio::test]
    async fn empty_db_yields_zero_counts() {
        let tmp = TempDir::new().unwrap();
        let pool = init_db(tmp.path()).await.unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();

        let opts = MigrationOptions {
            dry_run: false,
            backup: false,
            user_config_path: tmp.path().join("user.toml"),
        };
        let report = run_migration(&pool, &vault, &opts).await.unwrap();
        assert_eq!(report.connections_migrated, 0);
        assert_eq!(report.environments_migrated, 0);
        assert_eq!(report.variables_migrated, 0);
    }

    #[test]
    fn legacy_to_input_truncates_oversized_port() {
        let c = connections::Connection {
            id: "x".into(),
            name: "n".into(),
            driver: "postgres".into(),
            host: None,
            port: Some(70_000), // > u16
            database_name: None,
            username: None,
            password: None,
            ssl_mode: None,
            timeout_ms: 0,
            query_timeout_ms: 0,
            ttl_seconds: 0,
            max_pool_size: 0,
            is_readonly: false,
            last_tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let input = legacy_to_input(&c);
        assert!(input.port.is_none());
    }
}
