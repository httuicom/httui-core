//! `.httui/workspace.toml` schema.
//!
//! See ADR 0001. Strictly limited to collaboration-relevant defaults.
//! Visual settings live in `user.toml`, not here.

use serde::{Deserialize, Serialize};

use super::Version;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceFile {
    #[serde(default)]
    pub version: Version,

    #[serde(default)]
    pub defaults: WorkspaceDefaults,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceDefaults {
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub git_remote: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_workspace() {
        let raw = r#"
version = "1"
[defaults]
environment = "staging"
git_remote = "origin"
git_branch = "main"
"#;
        let f: WorkspaceFile = toml::from_str(raw).unwrap();
        assert_eq!(f.defaults.environment.as_deref(), Some("staging"));
        assert_eq!(f.defaults.git_remote.as_deref(), Some("origin"));
        assert_eq!(f.defaults.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn empty_workspace_defaults_to_v1() {
        let f: WorkspaceFile = toml::from_str(r#"version = "1""#).unwrap();
        assert_eq!(f.version, Version::V1);
        assert!(f.defaults.environment.is_none());
    }
}
