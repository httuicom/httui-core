//! Schema validation: anti-cleartext-secret checks and structural
//! constraints (e.g. `[secrets]` values must be references).
//!
//! Filled in story 02. This file currently exposes only the helpers
//! used by the type-safe parsers in story 01.

/// True when `s` is a `{{backend:address}}` secret reference (ADR 0002).
pub fn is_secret_ref(s: &str) -> bool {
    let trimmed = s.trim();
    if !(trimmed.starts_with("{{") && trimmed.ends_with("}}")) {
        return false;
    }
    let inner = &trimmed[2..trimmed.len() - 2];
    let Some((backend, rest)) = inner.split_once(':') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    matches!(backend, "keychain" | "1password" | "pass" | "env")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_keychain_ref() {
        assert!(is_secret_ref("{{keychain:ns:key}}"));
        assert!(is_secret_ref("{{1password:op://vault/item/field}}"));
        assert!(is_secret_ref("{{pass:work/db}}"));
        assert!(is_secret_ref("{{env:GITHUB_TOKEN}}"));
    }

    #[test]
    fn rejects_non_refs() {
        assert!(!is_secret_ref("literal value"));
        assert!(!is_secret_ref("{{NO_COLON}}"));
        assert!(!is_secret_ref("{{unknown:something}}"));
        assert!(!is_secret_ref("{{keychain:}}"));
        assert!(!is_secret_ref(""));
    }

    #[test]
    fn block_refs_are_not_secret_refs() {
        // {{alias.path}} is a block reference, not a secret reference.
        assert!(!is_secret_ref("{{req1.response.body.id}}"));
    }
}
