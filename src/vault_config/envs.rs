//! `envs/{name}.toml` schema.
//!
//! See ADR 0001. The split between `[vars]` (literals OK) and
//! `[secrets]` (must be `{{...}}` references) is structural. The
//! validator (story-02) enforces the constraint on `[secrets]`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Version;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvFile {
    #[serde(default)]
    pub version: Version,

    #[serde(default)]
    pub vars: BTreeMap<String, String>,

    #[serde(default)]
    pub secrets: BTreeMap<String, String>,

    #[serde(default)]
    pub meta: EnvMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvMeta {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub require_confirm: bool,
    #[serde(default)]
    pub color: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_env_file() {
        let raw = r#"
version = "1"

[vars]
BASE_URL = "https://api.staging.acme.dev"
TENANT_ID = "tnt_8f2a91"

[secrets]
ADMIN_TOKEN = "{{keychain:env:staging:ADMIN_TOKEN}}"
PG_PASSWORD = "{{keychain:env:staging:PG_PASSWORD}}"

[meta]
description = "Staging — Acme primary"
read_only = false
require_confirm = false
color = "amber"
"#;
        let f: EnvFile = toml::from_str(raw).unwrap();
        assert_eq!(f.version, Version::V1);
        assert_eq!(
            f.vars.get("BASE_URL").unwrap(),
            "https://api.staging.acme.dev"
        );
        assert_eq!(
            f.secrets.get("ADMIN_TOKEN").unwrap(),
            "{{keychain:env:staging:ADMIN_TOKEN}}"
        );
        assert_eq!(f.meta.color.as_deref(), Some("amber"));
    }

    #[test]
    fn empty_sections_default() {
        let raw = r#"version = "1""#;
        let f: EnvFile = toml::from_str(raw).unwrap();
        assert!(f.vars.is_empty());
        assert!(f.secrets.is_empty());
        assert!(!f.meta.read_only);
    }

    #[test]
    fn vars_and_secrets_are_independent() {
        let raw = r#"
version = "1"
[vars]
A = "literal"
[secrets]
B = "{{keychain:ns:k}}"
"#;
        let f: EnvFile = toml::from_str(raw).unwrap();
        assert_eq!(f.vars.len(), 1);
        assert_eq!(f.secrets.len(), 1);
    }
}
