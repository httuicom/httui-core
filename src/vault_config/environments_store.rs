//! File-backed environments store.
//!
//! Source of truth is `<vault_root>/envs/<name>.toml`. Each environment
//! lives in its own file with `[vars]` (literals OK) and `[secrets]`
//! (must be `{{...}}` references — see ADR 0001 / 0002). Active env
//! tracking lives in the per-machine `user.toml` so the same shared
//! vault can have different active envs on different machines.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::db::keychain::{delete_secret, env_var_key, resolve_secret_ref, store_secret};

use super::atomic::{read_toml, write_toml};
use super::envs::{EnvFile, EnvMeta};
use super::user::UserFile;
use super::validate::{validate_env_file, Severity};
use super::Version;

const ENVS_DIR: &str = "envs";

// --- DTOs --------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentPublic {
    pub name: String,
    pub description: Option<String>,
    pub read_only: bool,
    pub require_confirm: bool,
    pub color: Option<String>,
    pub var_count: usize,
    pub secret_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvVariablePublic {
    pub key: String,
    /// Plaintext value when from `[vars]`; empty string when from
    /// `[secrets]` (caller resolves on demand).
    pub value: String,
    pub is_secret: bool,
}

#[derive(Debug, Clone)]
pub struct SetVarInput {
    pub env_name: String,
    pub key: String,
    /// Raw value. For secret vars it's stored in the keychain and the
    /// TOML keeps only a `{{keychain:...}}` reference.
    pub value: String,
    pub is_secret: bool,
}

// --- Cache -------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CachedEnv {
    mtime: Option<SystemTime>,
    file: EnvFile,
}

/// File-backed CRUD over `envs/*.toml` plus active-env tracking in
/// `user.toml`.
pub struct EnvironmentsStore {
    vault_root: PathBuf,
    user_config_path: PathBuf,
    /// Per-env cache. Key is the env name (== filename stem).
    cache: RwLock<BTreeMap<String, CachedEnv>>,
}

impl EnvironmentsStore {
    pub fn new(vault_root: impl Into<PathBuf>, user_config_path: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            vault_root: vault_root.into(),
            user_config_path: user_config_path.into(),
            cache: RwLock::new(BTreeMap::new()),
        })
    }

    fn envs_dir(&self) -> PathBuf {
        self.vault_root.join(ENVS_DIR)
    }

    fn env_file_path(&self, name: &str) -> PathBuf {
        self.envs_dir().join(format!("{name}.toml"))
    }

    fn current_mtime(&self, path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
    }

    /// Load `<env>.toml`, using cache when on-disk mtime is unchanged.
    /// Returns `Ok(None)` when the file doesn't exist (env not created).
    async fn load_env(&self, name: &str) -> Result<Option<EnvFile>, String> {
        let path = self.env_file_path(name);
        if !path.exists() {
            return Ok(None);
        }
        let disk_mtime = self.current_mtime(&path);

        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(name) {
                if cached.mtime == disk_mtime {
                    return Ok(Some(cached.file.clone()));
                }
            }
        }

        let file: EnvFile =
            read_toml(&path).map_err(|e| format!("read {}: {e}", path.display()))?;

        let mut cache = self.cache.write().await;
        cache.insert(
            name.to_string(),
            CachedEnv {
                mtime: disk_mtime,
                file: file.clone(),
            },
        );
        Ok(Some(file))
    }

    /// Validate, write atomically, refresh cache.
    async fn persist_env(&self, name: &str, file: EnvFile) -> Result<(), String> {
        let report = validate_env_file(&file);
        if report.has_errors() {
            let summary = report
                .issues
                .iter()
                .filter(|i| i.severity == Severity::Error)
                .map(|i| format!("- {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "envs/{name}.toml refuses to save: validator found errors:\n{summary}"
            ));
        }
        let path = self.env_file_path(name);
        write_toml(&path, &file).map_err(|e| format!("write {}: {e}", path.display()))?;

        let mut cache = self.cache.write().await;
        cache.insert(
            name.to_string(),
            CachedEnv {
                mtime: self.current_mtime(&path),
                file,
            },
        );
        Ok(())
    }

    pub async fn invalidate_cache(&self) {
        let mut cache = self.cache.write().await;
        cache.clear();
    }

    // --- env-level CRUD -------------------------------------------------

    pub async fn list_envs(&self) -> Result<Vec<EnvironmentPublic>, String> {
        let dir = self.envs_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip *.local.toml (overrides — handled separately, ADR 0004)
            // and any non-.toml file.
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            if name.ends_with(".local") {
                continue;
            }
            if let Some(env) = self.load_env(name).await? {
                out.push(env_to_public(name, &env));
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub async fn get_env(&self, name: &str) -> Result<Option<EnvironmentPublic>, String> {
        Ok(self.load_env(name).await?.map(|f| env_to_public(name, &f)))
    }

    pub async fn create_env(&self, name: &str) -> Result<EnvironmentPublic, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("environment name is required".to_string());
        }
        if !is_valid_env_name(name) {
            return Err(format!(
                "invalid environment name '{name}' — use letters, digits, '-', '_'"
            ));
        }
        if self.env_file_path(name).exists() {
            return Err(format!("environment '{name}' already exists"));
        }
        let file = EnvFile {
            version: Version::V1,
            vars: BTreeMap::new(),
            secrets: BTreeMap::new(),
            meta: EnvMeta::default(),
        };
        self.persist_env(name, file.clone()).await?;
        Ok(env_to_public(name, &file))
    }

    pub async fn delete_env(&self, name: &str) -> Result<(), String> {
        let path = self.env_file_path(name);
        if !path.exists() {
            return Err(format!("environment '{name}' not found"));
        }
        // Best-effort keychain cleanup for every secret in this env.
        if let Some(file) = self.load_env(name).await? {
            for key in file.secrets.keys() {
                let _ = delete_secret(&env_var_key(name, key));
            }
        }
        std::fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))?;
        let mut cache = self.cache.write().await;
        cache.remove(name);
        Ok(())
    }

    // --- variable-level CRUD --------------------------------------------

    pub async fn list_vars(&self, env_name: &str) -> Result<Vec<EnvVariablePublic>, String> {
        let Some(file) = self.load_env(env_name).await? else {
            return Err(format!("environment '{env_name}' not found"));
        };
        let mut out = Vec::new();
        for (key, value) in &file.vars {
            out.push(EnvVariablePublic {
                key: key.clone(),
                value: value.clone(),
                is_secret: false,
            });
        }
        for key in file.secrets.keys() {
            out.push(EnvVariablePublic {
                key: key.clone(),
                value: String::new(),
                is_secret: true,
            });
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Resolve a var for actual execution. Secrets pass through the
    /// keychain. Plain `[vars]` come back verbatim.
    pub async fn resolve_var(&self, env_name: &str, key: &str) -> Result<Option<String>, String> {
        let Some(file) = self.load_env(env_name).await? else {
            return Ok(None);
        };
        if let Some(v) = file.vars.get(key) {
            return Ok(Some(v.clone()));
        }
        if let Some(reference) = file.secrets.get(key) {
            return match resolve_secret_ref(reference) {
                Ok(Some(v)) => Ok(Some(v)),
                Ok(None) => Err(format!("secret '{key}' has no keychain entry")),
                Err(e) => Err(format!("resolving secret '{key}': {e}")),
            };
        }
        Ok(None)
    }

    pub async fn set_var(&self, input: SetVarInput) -> Result<EnvVariablePublic, String> {
        let SetVarInput {
            env_name,
            key,
            value,
            is_secret,
        } = input;
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err("variable key is required".to_string());
        }
        let mut file = self
            .load_env(&env_name)
            .await?
            .ok_or_else(|| format!("environment '{env_name}' not found"))?;

        if is_secret {
            // Move plaintext to keychain; write only the reference into
            // the TOML.
            let kc_key = env_var_key(&env_name, &key);
            store_secret(&kc_key, &value)
                .map_err(|e| format!("Failed to store secret securely: {e}"))?;
            let reference = format!("{{{{keychain:{kc_key}}}}}");
            // If a same-named non-secret existed, remove it.
            file.vars.remove(&key);
            file.secrets.insert(key.clone(), reference);
        } else {
            // If a same-named secret existed, drop the keychain entry.
            if file.secrets.remove(&key).is_some() {
                let _ = delete_secret(&env_var_key(&env_name, &key));
            }
            file.vars.insert(key.clone(), value);
        }

        self.persist_env(&env_name, file).await?;
        Ok(EnvVariablePublic {
            key,
            value: String::new(),
            is_secret,
        })
    }

    pub async fn delete_var(&self, env_name: &str, key: &str) -> Result<(), String> {
        let mut file = self
            .load_env(env_name)
            .await?
            .ok_or_else(|| format!("environment '{env_name}' not found"))?;
        let removed_secret = file.secrets.remove(key).is_some();
        let removed_var = file.vars.remove(key).is_some();
        if !removed_secret && !removed_var {
            return Err(format!(
                "variable '{key}' not found in environment '{env_name}'"
            ));
        }
        if removed_secret {
            let _ = delete_secret(&env_var_key(env_name, key));
        }
        self.persist_env(env_name, file).await?;
        Ok(())
    }

    // --- active-env tracking (per-machine) ------------------------------

    fn read_user_file(&self) -> Result<UserFile, String> {
        if !self.user_config_path.exists() {
            return Ok(UserFile::default());
        }
        read_toml::<UserFile>(&self.user_config_path)
            .map_err(|e| format!("read {}: {e}", self.user_config_path.display()))
    }

    fn write_user_file(&self, f: &UserFile) -> Result<(), String> {
        write_toml(&self.user_config_path, f)
            .map_err(|e| format!("write {}: {e}", self.user_config_path.display()))
    }

    fn vault_key(&self) -> String {
        // Canonical key for the active_envs map: absolute vault path.
        self.vault_root
            .canonicalize()
            .unwrap_or_else(|_| self.vault_root.clone())
            .to_string_lossy()
            .into_owned()
    }

    pub async fn active_env(&self) -> Result<Option<String>, String> {
        let user = self.read_user_file()?;
        Ok(user.active_envs.get(&self.vault_key()).cloned())
    }

    pub async fn set_active_env(&self, name: Option<&str>) -> Result<(), String> {
        let mut user = self.read_user_file()?;
        let key = self.vault_key();
        match name {
            Some(n) if !n.trim().is_empty() => {
                if !self.env_file_path(n).exists() {
                    return Err(format!("environment '{n}' not found"));
                }
                user.active_envs.insert(key, n.to_string());
            }
            _ => {
                user.active_envs.remove(&key);
            }
        }
        self.write_user_file(&user)
    }
}

// --- conversion helpers --------------------------------------------------

fn env_to_public(name: &str, file: &EnvFile) -> EnvironmentPublic {
    EnvironmentPublic {
        name: name.to_string(),
        description: file.meta.description.clone(),
        read_only: file.meta.read_only,
        require_confirm: file.meta.require_confirm,
        color: file.meta.color.clone(),
        var_count: file.vars.len(),
        secret_count: file.secrets.len(),
    }
}

fn is_valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
// Tests serialize keychain access via KEYCHAIN_TEST_LOCK; the std Mutex
// guard is intentionally held across awaits to keep concurrent test
// runs deterministic. The lock is contention-free in practice (each
// test holds it for milliseconds).
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::db::keychain::KEYCHAIN_TEST_LOCK;
    use tempfile::TempDir;

    fn fresh_store() -> (Arc<EnvironmentsStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let user_path = tmp.path().join("user.toml");
        let store = EnvironmentsStore::new(tmp.path(), user_path);
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
        assert!(store.list_envs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_env_writes_file() {
        let (store, t) = fresh_store();
        let env = store.create_env("staging").await.unwrap();
        assert_eq!(env.name, "staging");
        assert_eq!(env.var_count, 0);
        assert!(t.path().join("envs/staging.toml").exists());

        let listed = store.list_envs().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "staging");
    }

    #[tokio::test]
    async fn create_env_rejects_duplicate() {
        let (store, _t) = fresh_store();
        let name = unique_name("dup");
        store.create_env(&name).await.unwrap();
        let err = store.create_env(&name).await.unwrap_err();
        assert!(err.contains("already exists"));
    }

    #[tokio::test]
    async fn create_env_rejects_invalid_name() {
        let (store, _t) = fresh_store();
        assert!(store.create_env("").await.is_err());
        assert!(store.create_env("   ").await.is_err());
        assert!(store.create_env("has spaces").await.is_err());
        assert!(store.create_env("../escape").await.is_err());
    }

    #[tokio::test]
    async fn set_plain_var_round_trip() {
        let (store, _t) = fresh_store();
        store.create_env("staging").await.unwrap();
        store
            .set_var(SetVarInput {
                env_name: "staging".into(),
                key: "BASE_URL".into(),
                value: "https://api.example.com".into(),
                is_secret: false,
            })
            .await
            .unwrap();

        let vars = store.list_vars("staging").await.unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].key, "BASE_URL");
        assert!(!vars[0].is_secret);
        assert_eq!(vars[0].value, "https://api.example.com");
    }

    #[tokio::test]
    async fn set_secret_var_keeps_value_off_disk() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, t) = fresh_store();
        let env_name = unique_name("sec-env");
        store.create_env(&env_name).await.unwrap();
        store
            .set_var(SetVarInput {
                env_name: env_name.clone(),
                key: "ADMIN_TOKEN".into(),
                value: "super-secret-value".into(),
                is_secret: true,
            })
            .await
            .unwrap();

        let raw = std::fs::read_to_string(t.path().join(format!("envs/{env_name}.toml"))).unwrap();
        assert!(!raw.contains("super-secret-value"));
        assert!(raw.contains(&format!("{{{{keychain:env:{env_name}:ADMIN_TOKEN}}}}")));

        let vars = store.list_vars(&env_name).await.unwrap();
        assert_eq!(vars.len(), 1);
        assert!(vars[0].is_secret);
        assert_eq!(vars[0].value, ""); // masked

        // Cleanup
        let _ = delete_secret(&env_var_key(&env_name, "ADMIN_TOKEN"));
    }

    #[tokio::test]
    async fn switching_var_kind_clears_old_storage() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        let (store, _t) = fresh_store();
        let env_name = unique_name("switch");
        store.create_env(&env_name).await.unwrap();

        // Start as plain var
        store
            .set_var(SetVarInput {
                env_name: env_name.clone(),
                key: "TOKEN".into(),
                value: "plain".into(),
                is_secret: false,
            })
            .await
            .unwrap();

        // Promote to secret — old plain entry must be gone from [vars]
        store
            .set_var(SetVarInput {
                env_name: env_name.clone(),
                key: "TOKEN".into(),
                value: "secret-now".into(),
                is_secret: true,
            })
            .await
            .unwrap();

        let vars = store.list_vars(&env_name).await.unwrap();
        assert_eq!(vars.len(), 1);
        assert!(vars[0].is_secret);

        // Demote back to plain — keychain entry must be deleted
        store
            .set_var(SetVarInput {
                env_name: env_name.clone(),
                key: "TOKEN".into(),
                value: "back-to-plain".into(),
                is_secret: false,
            })
            .await
            .unwrap();
        let vars = store.list_vars(&env_name).await.unwrap();
        assert!(!vars[0].is_secret);
    }

    #[tokio::test]
    async fn delete_var_removes_from_correct_section() {
        let (store, _t) = fresh_store();
        let env_name = unique_name("delvar");
        store.create_env(&env_name).await.unwrap();
        store
            .set_var(SetVarInput {
                env_name: env_name.clone(),
                key: "PUBLIC".into(),
                value: "x".into(),
                is_secret: false,
            })
            .await
            .unwrap();

        store.delete_var(&env_name, "PUBLIC").await.unwrap();
        assert!(store.list_vars(&env_name).await.unwrap().is_empty());

        let err = store.delete_var(&env_name, "NOPE").await.unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn delete_env_removes_file_and_clears_cache() {
        let (store, t) = fresh_store();
        let env_name = unique_name("delenv");
        store.create_env(&env_name).await.unwrap();
        let path = t.path().join(format!("envs/{env_name}.toml"));
        assert!(path.exists());

        store.delete_env(&env_name).await.unwrap();
        assert!(!path.exists());
        assert!(store.get_env(&env_name).await.unwrap().is_none());

        let err = store.delete_env(&env_name).await.unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn active_env_round_trip() {
        let (store, _t) = fresh_store();
        store.create_env("staging").await.unwrap();
        assert!(store.active_env().await.unwrap().is_none());

        store.set_active_env(Some("staging")).await.unwrap();
        assert_eq!(
            store.active_env().await.unwrap().as_deref(),
            Some("staging")
        );

        store.set_active_env(None).await.unwrap();
        assert!(store.active_env().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_active_rejects_unknown_env() {
        let (store, _t) = fresh_store();
        let err = store
            .set_active_env(Some("does-not-exist"))
            .await
            .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn list_envs_skips_local_overrides() {
        let (store, t) = fresh_store();
        store.create_env("staging").await.unwrap();
        // Manually drop a staging.local.toml — it should not appear in list.
        std::fs::write(
            t.path().join("envs/staging.local.toml"),
            r#"version = "1"
[vars]
BASE_URL = "http://localhost"
"#,
        )
        .unwrap();
        let envs = store.list_envs().await.unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].name, "staging");
    }

    #[tokio::test]
    async fn resolve_var_returns_plain_value() {
        let (store, _t) = fresh_store();
        store.create_env("staging").await.unwrap();
        store
            .set_var(SetVarInput {
                env_name: "staging".into(),
                key: "HOST".into(),
                value: "api.example.com".into(),
                is_secret: false,
            })
            .await
            .unwrap();
        let resolved = store.resolve_var("staging", "HOST").await.unwrap();
        assert_eq!(resolved.as_deref(), Some("api.example.com"));

        // Missing key
        assert!(store
            .resolve_var("staging", "MISSING")
            .await
            .unwrap()
            .is_none());
        // Missing env
        assert!(store.resolve_var("nope", "X").await.unwrap().is_none());
    }
}
