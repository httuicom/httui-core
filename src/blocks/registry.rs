use std::collections::HashMap;

/// Maps block type aliases to their canonical executor names.
///
/// Block types in markdown (`db-postgres`, `db-mysql`, `graphql-http`, …)
/// can carry presentation hints that shouldn't leak into the executor
/// dispatch. The registry decouples the surface form from the executor key,
/// letting new block types and dialects register without touching the runner.
///
/// Defaults shipped with the core: `http` and the `db` family
/// (`db`, `db-postgres`, `db-mysql`, `db-sqlite` → `db`).
pub struct BlockTypeRegistry {
    aliases: HashMap<String, String>,
}

impl BlockTypeRegistry {
    pub fn new() -> Self {
        Self {
            aliases: HashMap::new(),
        }
    }

    /// Registry pre-populated with the block types currently shipped with
    /// the core.
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register_alias("http", "http");
        r.register_alias("db", "db");
        r.register_alias("db-postgres", "db");
        r.register_alias("db-mysql", "db");
        r.register_alias("db-sqlite", "db");
        r
    }

    /// Map an alias to its canonical executor name.
    pub fn register_alias(&mut self, alias: &str, canonical: &str) {
        self.aliases
            .insert(alias.to_string(), canonical.to_string());
    }

    /// Return the canonical executor name for a given block type, falling
    /// back to the input itself when no alias is registered. This makes
    /// unknown types pass through unchanged — a forward-compat hook for
    /// experimental block types not yet wired into the registry.
    pub fn canonicalize<'a>(&'a self, block_type: &'a str) -> &'a str {
        self.aliases
            .get(block_type)
            .map(String::as_str)
            .unwrap_or(block_type)
    }

    pub fn is_registered(&self, block_type: &str) -> bool {
        self.aliases.contains_key(block_type)
    }
}

impl Default for BlockTypeRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_canonicalize_db_family() {
        let r = BlockTypeRegistry::with_defaults();
        assert_eq!(r.canonicalize("db"), "db");
        assert_eq!(r.canonicalize("db-postgres"), "db");
        assert_eq!(r.canonicalize("db-mysql"), "db");
        assert_eq!(r.canonicalize("db-sqlite"), "db");
    }

    #[test]
    fn defaults_passthrough_http() {
        let r = BlockTypeRegistry::with_defaults();
        assert_eq!(r.canonicalize("http"), "http");
    }

    #[test]
    fn unknown_passes_through() {
        let r = BlockTypeRegistry::with_defaults();
        assert_eq!(r.canonicalize("graphql"), "graphql");
        assert!(!r.is_registered("graphql"));
    }

    #[test]
    fn custom_alias_canonicalizes() {
        let mut r = BlockTypeRegistry::with_defaults();
        r.register_alias("graphql-http", "graphql");
        assert_eq!(r.canonicalize("graphql-http"), "graphql");
        assert!(r.is_registered("graphql-http"));
    }

    #[test]
    fn empty_registry_passes_everything_through() {
        let r = BlockTypeRegistry::new();
        assert_eq!(r.canonicalize("http"), "http");
        assert_eq!(r.canonicalize("anything"), "anything");
        assert!(!r.is_registered("http"));
    }
}
