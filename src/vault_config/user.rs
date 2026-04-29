//! `~/.config/httui/user.toml` schema.
//!
//! See ADR 0001. Per-machine, never synced. Holds visual prefs,
//! shortcuts, secrets backend choice.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Version;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserFile {
    #[serde(default)]
    pub version: Version,

    #[serde(default)]
    pub ui: UiPrefs,

    #[serde(default)]
    pub shortcuts: BTreeMap<String, String>,

    #[serde(default)]
    pub secrets: SecretsBackend,

    #[serde(default)]
    pub mcp: McpConfig,

    /// Active environment per vault, keyed by absolute vault path.
    /// Per-machine state — never committed to git. Read by
    /// `EnvironmentsStore::active_env(vault_path)`.
    #[serde(default)]
    pub active_envs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPrefs {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: u16,
    #[serde(default = "default_density")]
    pub density: String,
    /// Editor auto-save debounce window in milliseconds.
    /// MVP `app_config` key: `auto_save_ms`.
    #[serde(default = "default_auto_save_ms")]
    pub auto_save_ms: u32,
    /// DB block default `LIMIT` when the user hasn't explicitly
    /// pinned one. MVP `app_config` key: `default_fetch_size`.
    #[serde(default = "default_fetch_size")]
    pub default_fetch_size: u32,
    /// Per-block history retention cap. MVP `app_config` key:
    /// `history_retention`.
    #[serde(default = "default_history_retention")]
    pub history_retention: u32,
    /// Editor vim-mode toggle. MVP `app_config` key: `vim_enabled`.
    #[serde(default)]
    pub vim_enabled: bool,
    /// Sidebar open/closed. MVP `app_config` key: `sidebar_open`.
    #[serde(default = "default_sidebar_open")]
    pub sidebar_open: bool,
    /// Color mode preference: `"system"` | `"light"` | `"dark"`. The
    /// frontend wires this to Chakra's color mode + `<html class>` so
    /// `lib/theme.ts` semanticTokens resolve via `_dark` / `_light`.
    /// Distinct from `theme` (legacy customisation JSON; pending
    /// reframe in Epic 19 Story 01 sweep).
    #[serde(default = "default_color_mode")]
    pub color_mode: String,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            density: default_density(),
            auto_save_ms: default_auto_save_ms(),
            default_fetch_size: default_fetch_size(),
            history_retention: default_history_retention(),
            vim_enabled: false,
            sidebar_open: default_sidebar_open(),
            color_mode: default_color_mode(),
        }
    }
}

fn default_theme() -> String {
    "system".to_string()
}
fn default_font_family() -> String {
    "JetBrains Mono".to_string()
}
fn default_font_size() -> u16 {
    14
}
fn default_density() -> String {
    "comfortable".to_string()
}
fn default_auto_save_ms() -> u32 {
    1000
}
fn default_fetch_size() -> u32 {
    100
}
fn default_history_retention() -> u32 {
    10
}
fn default_sidebar_open() -> bool {
    true
}
fn default_color_mode() -> String {
    "system".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsBackend {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_biometric")]
    pub biometric: bool,
    #[serde(default = "default_prompt_timeout")]
    pub prompt_timeout_s: u32,
}

impl Default for SecretsBackend {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            biometric: default_biometric(),
            prompt_timeout_s: default_prompt_timeout(),
        }
    }
}

fn default_backend() -> String {
    "auto".to_string()
}
fn default_biometric() -> bool {
    true
}
fn default_prompt_timeout() -> u32 {
    60
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, toml::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_user_file() {
        let raw = r#"
version = "1"

[ui]
theme = "dark"
font_family = "Fira Code"
font_size = 13
density = "compact"

[shortcuts]
"toggle.sidebar" = "Cmd+B"

[secrets]
backend = "1password"
biometric = true
prompt_timeout_s = 30
"#;
        let f: UserFile = toml::from_str(raw).unwrap();
        assert_eq!(f.ui.theme, "dark");
        assert_eq!(f.ui.font_family, "Fira Code");
        assert_eq!(f.ui.font_size, 13);
        assert_eq!(f.shortcuts.get("toggle.sidebar").unwrap(), "Cmd+B");
        assert_eq!(f.secrets.backend, "1password");
    }

    #[test]
    fn empty_user_file_yields_defaults() {
        let f: UserFile = toml::from_str("").unwrap();
        assert_eq!(f.ui.theme, "system");
        assert_eq!(f.ui.font_size, 14);
        assert_eq!(f.ui.color_mode, "system");
        assert_eq!(f.secrets.backend, "auto");
        assert!(f.secrets.biometric);
        assert!(f.active_envs.is_empty());
    }

    #[test]
    fn color_mode_round_trips() {
        let raw = "version = \"1\"\n[ui]\ncolor_mode = \"dark\"\n";
        let f: UserFile = toml::from_str(raw).unwrap();
        assert_eq!(f.ui.color_mode, "dark");

        let serialized = toml::to_string(&f).unwrap();
        assert!(serialized.contains("color_mode = \"dark\""));

        let back: UserFile = toml::from_str(&serialized).unwrap();
        assert_eq!(back.ui.color_mode, "dark");
    }

    #[test]
    fn ui_prefs_default_populates_every_field() {
        let p = UiPrefs::default();
        assert_eq!(p.theme, "system");
        assert_eq!(p.font_family, "JetBrains Mono");
        assert_eq!(p.font_size, 14);
        assert_eq!(p.density, "comfortable");
        assert_eq!(p.auto_save_ms, 1000);
        assert_eq!(p.default_fetch_size, 100);
        assert_eq!(p.history_retention, 10);
        assert!(!p.vim_enabled);
        assert!(p.sidebar_open);
        assert_eq!(p.color_mode, "system");
    }

    #[test]
    fn secrets_backend_default_matches_documented_values() {
        let b = SecretsBackend::default();
        assert_eq!(b.backend, "auto");
        assert!(b.biometric);
        assert_eq!(b.prompt_timeout_s, 60);
    }

    #[test]
    fn empty_user_file_serialises_back_to_default_round_trip() {
        let original: UserFile = toml::from_str("").unwrap();
        let serialised = toml::to_string(&original).unwrap();
        let reparsed: UserFile = toml::from_str(&serialised).unwrap();
        assert_eq!(reparsed.ui.theme, "system");
        assert_eq!(reparsed.ui.color_mode, "system");
        assert_eq!(reparsed.secrets.backend, "auto");
    }

    #[test]
    fn mcp_config_round_trips_servers_table() {
        let raw = r#"
version = "1"
[mcp.servers."notes-mcp"]
command = "httui-mcp"
"#;
        let f: UserFile = toml::from_str(raw).unwrap();
        assert!(f.mcp.servers.contains_key("notes-mcp"));

        let mcp_default = McpConfig::default();
        assert!(mcp_default.servers.is_empty());
    }

    #[test]
    fn active_envs_round_trip() {
        let raw = r#"
version = "1"
[active_envs]
"/Users/me/work" = "staging"
"/Users/me/personal" = "local"
"#;
        let f: UserFile = toml::from_str(raw).unwrap();
        assert_eq!(f.active_envs.get("/Users/me/work").unwrap(), "staging");
        assert_eq!(f.active_envs.get("/Users/me/personal").unwrap(), "local");

        let serialized = toml::to_string(&f).unwrap();
        let reparsed: UserFile = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.active_envs.len(), 2);
    }
}
