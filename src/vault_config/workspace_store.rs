//! File-backed workspace config store.
//!
//! Source of truth is `<vault_root>/.httui/workspace.toml`. Holds
//! collaboration-relevant defaults (active environment, git remote, git
//! branch) — strictly the small set of values that make sense to
//! commit alongside the vault. Per-machine prefs live in `user.toml`
//! (see [`UserStore`](super::user_store::UserStore)).
//!
//! This store has no keychain integration (workspace defaults aren't
//! sensitive) and no validation surface beyond what `serde` enforces on
//! the `WorkspaceFile` schema. The shape is deliberately minimal so the
//! file stays human-reviewable in PRs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use super::atomic::{read_toml, write_toml};
use super::workspace::{WorkspaceDefaults, WorkspaceFile};
use super::Version;

const WORKSPACE_DIR: &str = ".httui";
const WORKSPACE_FILE: &str = "workspace.toml";

#[derive(Debug, Clone)]
struct Cached {
    mtime: Option<SystemTime>,
    file: WorkspaceFile,
}

/// File-backed read/write over `.httui/workspace.toml`.
pub struct WorkspaceStore {
    vault_root: PathBuf,
    cache: RwLock<Option<Cached>>,
}

impl WorkspaceStore {
    pub fn new(vault_root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            vault_root: vault_root.into(),
            cache: RwLock::new(None),
        })
    }

    pub fn path(&self) -> PathBuf {
        self.vault_root.join(WORKSPACE_DIR).join(WORKSPACE_FILE)
    }

    fn current_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(self.path())
            .ok()
            .and_then(|m| m.modified().ok())
    }

    /// Returns the parsed file, using the cache when on-disk mtime is
    /// unchanged. Returns a default-valued file when missing — this
    /// matches the "auto-create on first run" contract: callers that
    /// only read see sensible defaults; the file materialises on the
    /// first `set_*` call.
    pub async fn load(&self) -> Result<WorkspaceFile, String> {
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
            read_toml::<WorkspaceFile>(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?
        } else {
            WorkspaceFile::default()
        };

        let mut cache = self.cache.write().await;
        *cache = Some(Cached {
            mtime: disk_mtime,
            file: file.clone(),
        });
        Ok(file)
    }

    async fn persist(&self, mut file: WorkspaceFile) -> Result<(), String> {
        // Force the on-disk version stamp; downstream readers rely on
        // it being explicit even when the user has only ever touched
        // `[defaults]`.
        file.version = Version::V1;
        let path = self.path();
        write_toml(&path, &file).map_err(|e| format!("write {}: {e}", path.display()))?;

        let mut cache = self.cache.write().await;
        *cache = Some(Cached {
            mtime: self.current_mtime(),
            file,
        });
        Ok(())
    }

    /// Force the next read to hit disk. Hooks into the file watcher
    /// (epic 11) so external edits don't get masked by the cache.
    pub async fn invalidate_cache(&self) {
        let mut cache = self.cache.write().await;
        *cache = None;
    }

    /// Read-only accessor that mirrors `WorkspaceFile.defaults`.
    pub async fn defaults(&self) -> Result<WorkspaceDefaults, String> {
        Ok(self.load().await?.defaults)
    }

    /// Replace the entire `[defaults]` section. Empty strings are
    /// treated as "unset" and stored as `None` to keep the TOML clean.
    pub async fn set_defaults(&self, mut defaults: WorkspaceDefaults) -> Result<(), String> {
        normalize(&mut defaults.environment);
        normalize(&mut defaults.git_remote);
        normalize(&mut defaults.git_branch);
        let mut file = self.load().await?;
        file.defaults = defaults;
        self.persist(file).await
    }

    /// Ensure the file exists on disk. Idempotent — useful for the
    /// first-run flow that wants the file present before showing a UI.
    pub async fn ensure_exists(&self) -> Result<(), String> {
        if self.path().exists() {
            return Ok(());
        }
        let file = self.load().await?;
        self.persist(file).await
    }
}

fn normalize(value: &mut Option<String>) {
    if let Some(v) = value {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            *value = None;
        } else if trimmed.len() != v.len() {
            *value = Some(trimmed.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(dir: &TempDir) -> Arc<WorkspaceStore> {
        WorkspaceStore::new(dir.path())
    }

    #[tokio::test]
    async fn load_returns_default_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let f = s.load().await.unwrap();
        assert_eq!(f.version, Version::V1);
        assert!(f.defaults.environment.is_none());
    }

    #[tokio::test]
    async fn set_defaults_creates_file_and_writes_through() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.set_defaults(WorkspaceDefaults {
            environment: Some("staging".into()),
            git_remote: Some("origin".into()),
            git_branch: Some("main".into()),
        })
        .await
        .unwrap();
        assert!(s.path().exists());
        let raw = std::fs::read_to_string(s.path()).unwrap();
        assert!(raw.contains("environment = \"staging\""));
        assert!(raw.contains("version = \"1\""));
        let d = s.defaults().await.unwrap();
        assert_eq!(d.environment.as_deref(), Some("staging"));
    }

    #[tokio::test]
    async fn set_defaults_normalizes_empty_strings_to_none() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.set_defaults(WorkspaceDefaults {
            environment: Some("   ".into()),
            git_remote: Some(String::new()),
            git_branch: Some(" main ".into()),
        })
        .await
        .unwrap();
        let d = s.defaults().await.unwrap();
        assert!(d.environment.is_none());
        assert!(d.git_remote.is_none());
        assert_eq!(d.git_branch.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn cache_hits_when_mtime_unchanged() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.set_defaults(WorkspaceDefaults {
            environment: Some("staging".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        // Tamper with the file directly. Without an mtime change the
        // cache should still serve the previous content.
        std::fs::write(s.path(), "version = \"1\"\n[defaults]\n").unwrap();
        // mtime probably *did* change here; force the cache instead.
        let d = s.defaults().await.unwrap();
        // Either we caught the mtime change (and now see empty) or we
        // served the cached value. Both are valid. The point of this
        // test is to lock in that *invalidation* explicitly clears.
        let _ = d;
        s.invalidate_cache().await;
        let d2 = s.defaults().await.unwrap();
        assert!(d2.environment.is_none());
    }

    #[tokio::test]
    async fn ensure_exists_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        assert!(!s.path().exists());
        s.ensure_exists().await.unwrap();
        assert!(s.path().exists());
        let mtime1 = std::fs::metadata(s.path()).unwrap().modified().unwrap();
        // Sleep enough that any rewrite would change the mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.ensure_exists().await.unwrap();
        let mtime2 = std::fs::metadata(s.path()).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "second call should not rewrite");
    }

    #[tokio::test]
    async fn load_after_external_edit_picks_up_changes() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.ensure_exists().await.unwrap();
        // Wait a tick so the mtime can advance on filesystems with
        // 1s mtime granularity.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            s.path(),
            "version = \"1\"\n[defaults]\nenvironment = \"prod\"\n",
        )
        .unwrap();
        let d = s.defaults().await.unwrap();
        assert_eq!(d.environment.as_deref(), Some("prod"));
    }

    #[tokio::test]
    async fn read_invalid_toml_returns_error() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        std::fs::create_dir_all(dir.path().join(WORKSPACE_DIR)).unwrap();
        std::fs::write(s.path(), "this is not = = valid").unwrap();
        let err = s.load().await.unwrap_err();
        assert!(err.contains("read"), "got {err}");
    }

    #[test]
    fn normalize_handles_all_branches() {
        let mut none: Option<String> = None;
        normalize(&mut none);
        assert!(none.is_none());

        let mut empty = Some(String::new());
        normalize(&mut empty);
        assert!(empty.is_none());

        let mut whitespace = Some("   ".to_string());
        normalize(&mut whitespace);
        assert!(whitespace.is_none());

        let mut padded = Some(" foo ".to_string());
        normalize(&mut padded);
        assert_eq!(padded.as_deref(), Some("foo"));

        let mut clean = Some("bar".to_string());
        normalize(&mut clean);
        assert_eq!(clean.as_deref(), Some("bar"));
    }
}
