//! Filesystem layout shared across the three binaries.
//!
//! All three (desktop, TUI, MCP) now converge on the desktop's Tauri
//! namespace `com.notes.app` so they share the same SQLite database
//! (connections, environments, run history, etc.). The previous
//! `com.httui.notes` namespace is kept as a fallback when an existing
//! install has data there but the desktop hasn't been launched yet.

use std::path::PathBuf;

use crate::error::{CoreError, CoreResult};

const PRIMARY_NAMESPACE: &str = "com.notes.app";
const LEGACY_NAMESPACE: &str = "com.httui.notes";

/// Default filesystem location for shared application data
/// (SQLite database, schema cache, vault registry).
///
/// Resolution order:
/// 1. `<app_support>/com.notes.app` — the desktop binary's Tauri
///    namespace. Used unconditionally when the directory exists or
///    when no legacy install is present, so a fresh TUI install
///    creates state in the same place the desktop will read from.
/// 2. `<app_support>/com.httui.notes` — legacy fallback. Used only
///    when it already has a `notes.db` and the primary namespace
///    doesn't, so existing TUI users keep their data until they
///    migrate (or launch the desktop, which will populate primary).
pub fn default_data_dir() -> CoreResult<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| CoreError::Other("neither HOME nor USERPROFILE is set".into()))?;
    let home = PathBuf::from(home);

    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support");

    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join("AppData/Roaming"));

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/share"));

    let primary = base.join(PRIMARY_NAMESPACE);
    let legacy = base.join(LEGACY_NAMESPACE);

    // Prefer legacy ONLY when it has a populated database and primary
    // doesn't — keeps existing TUI installs working without forcing a
    // migration.
    if !primary.join("notes.db").exists() && legacy.join("notes.db").exists() {
        return Ok(legacy);
    }
    Ok(primary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_dir_uses_a_known_namespace() {
        let p = default_data_dir().unwrap();
        let s = p.to_string_lossy();
        assert!(
            s.contains(PRIMARY_NAMESPACE) || s.contains(LEGACY_NAMESPACE),
            "{p:?} must contain {PRIMARY_NAMESPACE} or {LEGACY_NAMESPACE}"
        );
    }
}
