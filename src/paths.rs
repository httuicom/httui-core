//! Filesystem layout shared across the three binaries.
//!
//! Today MCP and TUI converge on `com.httui.notes`; the desktop binary
//! historically uses `com.notes.app` (Tauri default). Unifying all
//! three is tracked under Epic 22 (Co-existência Desktop ↔ TUI).
//! Until then, callers that want cross-binary state sharing must use
//! the same path family.

use std::path::PathBuf;

use crate::error::{CoreError, CoreResult};

const APP_NAMESPACE: &str = "com.httui.notes";

/// Default filesystem location for shared application data
/// (SQLite database, schema cache, vault registry).
///
/// - macOS: `~/Library/Application Support/com.httui.notes`
/// - Linux / BSD: `$XDG_DATA_HOME/com.httui.notes` (falls back to
///   `~/.local/share/com.httui.notes`)
/// - Windows: `%APPDATA%\com.httui.notes`
pub fn default_data_dir() -> CoreResult<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| CoreError::Other("neither HOME nor USERPROFILE is set".into()))?;
    let home = PathBuf::from(home);

    #[cfg(target_os = "macos")]
    let path = home.join("Library/Application Support").join(APP_NAMESPACE);

    #[cfg(target_os = "windows")]
    let path = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join("AppData/Roaming"))
        .join(APP_NAMESPACE);

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let path = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".local/share"))
        .join(APP_NAMESPACE);

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_dir_contains_namespace() {
        let p = default_data_dir().unwrap();
        assert!(
            p.to_string_lossy().contains(APP_NAMESPACE),
            "{p:?} must contain {APP_NAMESPACE}"
        );
    }
}
