//! Vault file/directory layout constants.
//!
//! Single source of truth for the on-disk names that make up a vault.
//! Keeps the magic strings out of every store + scaffold + migration
//! file. Match ADR 0001's directory contract.
//!
//! Adding a new top-level vault file/directory? Define its constant
//! here and import from one place — no scattered string literals.

/// `<vault_root>/connections.toml` — connection definitions.
pub const CONNECTIONS_FILE: &str = "connections.toml";

/// `<vault_root>/envs/` — per-environment vars and secrets.
pub const ENVS_DIR: &str = "envs";

/// `<vault_root>/.httui/` — workspace-scoped config (workspace.toml,
/// gitignored sweep state, etc.). Hidden by convention to keep clutter
/// out of the user's vault root.
pub const WORKSPACE_DIR: &str = ".httui";

/// `<vault_root>/.httui/workspace.toml` — workspace defaults
/// (active environment, git remote/branch, …) that travel with the
/// vault in version control.
pub const WORKSPACE_FILE: &str = "workspace.toml";

/// Suffix used for `*.local.toml` overrides (ADR 0004 — local
/// overrides for any `*.toml` in the vault). Stored alongside the
/// base file; gitignored by `scaffold::write_gitignore`.
pub const LOCAL_TOML_SUFFIX: &str = ".local.toml";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_adr_0001_contract() {
        assert_eq!(CONNECTIONS_FILE, "connections.toml");
        assert_eq!(ENVS_DIR, "envs");
        assert_eq!(WORKSPACE_DIR, ".httui");
        assert_eq!(WORKSPACE_FILE, "workspace.toml");
        assert_eq!(LOCAL_TOML_SUFFIX, ".local.toml");
    }
}
