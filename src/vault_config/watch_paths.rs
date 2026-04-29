//! Pure path classifier for the file watcher (ADR 0003).
//!
//! Given a path that fired the OS-level watcher, decide which
//! category — if any — it belongs to. Returning `None` means "ignore
//! this path", which lets the dispatcher skip it without ceremony.
//!
//! The classifier is intentionally string-only — no I/O, no
//! `std::path::Path` canonicalisation. Watcher events arrive frequently
//! and we want this hot path to be allocation-light and trivially
//! testable.

/// What kind of config file changed. Mirrors the categories from
/// ADR 0003. `Local` siblings collapse into the same category as the
/// base — the resolver re-merges either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchCategory {
    /// `connections.toml` or `connections.local.toml`.
    Connections,
    /// `envs/<name>.toml` or `envs/<name>.local.toml`.
    Env,
    /// `.httui/workspace.toml` or `.httui/workspace.local.toml`.
    Workspace,
}

/// Classify `vault_relative` (a path relative to the vault root, with
/// forward slashes — caller normalises) into a watch category. Returns
/// `None` for any path that isn't a watched config file.
pub fn classify(vault_relative: &str) -> Option<WatchCategory> {
    // Strip a leading `./` if present.
    let p = vault_relative.strip_prefix("./").unwrap_or(vault_relative);

    // 1. workspace.toml lives under .httui/
    if p == ".httui/workspace.toml" || p == ".httui/workspace.local.toml" {
        return Some(WatchCategory::Workspace);
    }

    // 2. connections.toml at vault root
    if p == "connections.toml" || p == "connections.local.toml" {
        return Some(WatchCategory::Connections);
    }

    // 3. envs/*.toml + envs/*.local.toml
    if let Some(rest) = p.strip_prefix("envs/") {
        // Reject nested directories (envs/foo/bar.toml) — flat layout
        // only per ADR 0001.
        if rest.contains('/') {
            return None;
        }
        // Must end in `.toml`. `.local.toml` is also fine.
        if rest.ends_with(".toml") {
            return Some(WatchCategory::Env);
        }
    }

    None
}

/// Extract the env name from a path classified as
/// [`WatchCategory::Env`]. `envs/staging.toml` → `Some("staging")`,
/// `envs/staging.local.toml` → `Some("staging")`. Returns `None` for
/// any path that wasn't an env file in the first place — caller
/// already validated via `classify`.
pub fn env_name_from_path(vault_relative: &str) -> Option<&str> {
    let p = vault_relative.strip_prefix("./").unwrap_or(vault_relative);
    let rest = p.strip_prefix("envs/")?;
    if rest.contains('/') {
        return None;
    }
    let stem = rest.strip_suffix(".toml")?;
    let stem = stem.strip_suffix(".local").unwrap_or(stem);
    if stem.is_empty() {
        return None;
    }
    Some(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_workspace_files() {
        assert_eq!(
            classify(".httui/workspace.toml"),
            Some(WatchCategory::Workspace)
        );
        assert_eq!(
            classify(".httui/workspace.local.toml"),
            Some(WatchCategory::Workspace)
        );
    }

    #[test]
    fn classifies_connections_files() {
        assert_eq!(
            classify("connections.toml"),
            Some(WatchCategory::Connections)
        );
        assert_eq!(
            classify("connections.local.toml"),
            Some(WatchCategory::Connections)
        );
    }

    #[test]
    fn classifies_env_files() {
        assert_eq!(classify("envs/staging.toml"), Some(WatchCategory::Env));
        assert_eq!(
            classify("envs/staging.local.toml"),
            Some(WatchCategory::Env)
        );
        assert_eq!(classify("envs/dev.toml"), Some(WatchCategory::Env));
    }

    #[test]
    fn rejects_unknown_paths() {
        assert_eq!(classify("notes.md"), None);
        assert_eq!(classify("Cargo.toml"), None);
        assert_eq!(classify(".gitignore"), None);
        assert_eq!(classify(".httui/cache/foo.json"), None);
        assert_eq!(classify(".httui/other.toml"), None);
    }

    #[test]
    fn rejects_nested_env_paths() {
        assert_eq!(classify("envs/group/staging.toml"), None);
    }

    #[test]
    fn rejects_envs_dir_with_non_toml() {
        assert_eq!(classify("envs/staging.txt"), None);
        assert_eq!(classify("envs/staging"), None);
    }

    #[test]
    fn handles_dot_slash_prefix() {
        assert_eq!(
            classify("./connections.toml"),
            Some(WatchCategory::Connections)
        );
    }

    #[test]
    fn env_name_from_base_file() {
        assert_eq!(env_name_from_path("envs/staging.toml"), Some("staging"));
    }

    #[test]
    fn env_name_from_local_file() {
        assert_eq!(
            env_name_from_path("envs/staging.local.toml"),
            Some("staging")
        );
    }

    #[test]
    fn env_name_returns_none_for_non_env() {
        assert_eq!(env_name_from_path("connections.toml"), None);
        assert_eq!(env_name_from_path(".httui/workspace.toml"), None);
        assert_eq!(env_name_from_path("envs/group/x.toml"), None);
        assert_eq!(env_name_from_path("envs/.toml"), None);
    }

    #[test]
    fn env_name_handles_dot_slash() {
        assert_eq!(env_name_from_path("./envs/staging.toml"), Some("staging"));
    }
}
