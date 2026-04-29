//! Vault configuration files (TOML).
//!
//! Implements the schemas defined in ADR 0001 (`docs-llm/v1/adr/`):
//!
//! - `connections.toml` — connection definitions
//! - `envs/{name}.toml` — per-environment vars and secrets
//! - `.httui/workspace.toml` — workspace defaults
//! - `~/.config/httui/user.toml` — per-machine user prefs
//!
//! Plus `*.local.toml` overrides, handled by the merge layer (ADR 0004,
//! built in a later epic).

pub mod atomic;
pub mod connections;
pub mod envs;
pub mod user;
pub mod validate;
pub mod workspace;

pub use connections::{Connection, ConnectionsFile};
pub use envs::{EnvFile, EnvMeta};
pub use user::UserFile;
pub use workspace::WorkspaceFile;

use serde::{Deserialize, Serialize};

/// Schema version stamped at the top of every vault TOML file.
///
/// Bump only on breaking schema changes. Files without an explicit
/// `version` field default to `V1` (grandfathered for v1 itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Version {
    #[default]
    #[serde(rename = "1")]
    V1,
}
