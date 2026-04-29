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
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            density: default_density(),
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
        assert_eq!(f.secrets.backend, "auto");
        assert!(f.secrets.biometric);
    }
}
