//! Auto-augment a vault's `.gitignore` with the canonical
//! `*.local.toml` lines (ADR 0004).
//!
//! Behaviour:
//!
//! - If `.gitignore` is missing, write a fresh one with just our block.
//! - If `.gitignore` exists, append our block at the end if **none** of
//!   our patterns appear yet. We don't try to dedupe individual
//!   patterns — partial matches stay alone, full block goes in once.
//! - We never reorganise the rest of the file.

use std::path::{Path, PathBuf};

const HTTUI_BLOCK_HEADER: &str = "# httui local overrides — never commit these";
const HTTUI_PATTERNS: &[&str] = &[
    "envs/*.local.toml",
    "connections.local.toml",
    ".httui/workspace.local.toml",
    ".httui/cache/",
];

/// Result describing whether `.gitignore` was created, augmented, or
/// left alone. Useful for telemetry and the UI's "we just touched
/// your gitignore" toast.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitignoreOutcome {
    Created,
    Augmented,
    AlreadyPresent,
}

/// Ensure `vault_root/.gitignore` contains the httui block. Idempotent.
pub fn ensure_local_overrides_in_gitignore(vault_root: &Path) -> std::io::Result<GitignoreOutcome> {
    let path = vault_root.join(".gitignore");

    if !path.exists() {
        let body = render_block();
        write_atomic_or_inplace(&path, &body)?;
        return Ok(GitignoreOutcome::Created);
    }

    let existing = std::fs::read_to_string(&path)?;
    if all_patterns_present(&existing) {
        return Ok(GitignoreOutcome::AlreadyPresent);
    }

    // Append our full block. Preserve a trailing newline if it already
    // exists; otherwise insert one so our header lands on its own line.
    let mut next = existing;
    if !next.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str(&render_block());
    write_atomic_or_inplace(&path, &next)?;
    Ok(GitignoreOutcome::Augmented)
}

fn render_block() -> String {
    let mut out = String::with_capacity(256);
    out.push_str(HTTUI_BLOCK_HEADER);
    out.push('\n');
    for p in HTTUI_PATTERNS {
        out.push_str(p);
        out.push('\n');
    }
    out
}

fn all_patterns_present(existing: &str) -> bool {
    HTTUI_PATTERNS.iter().all(|p| line_present(existing, p))
}

fn line_present(haystack: &str, needle: &str) -> bool {
    haystack.lines().any(|l| l.trim() == needle)
}

/// Reuse the atomic-write helper when possible; otherwise fall through
/// to a plain write. `write_atomic` lives in the sibling `atomic`
/// module but only handles `&str` content. Same here.
fn write_atomic_or_inplace(path: &Path, content: &str) -> std::io::Result<()> {
    super::atomic::write_atomic(path, content)
}

/// Convenience: discover the `.gitignore` path for a vault.
pub fn gitignore_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".gitignore")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_gitignore_when_missing() {
        let dir = TempDir::new().unwrap();
        let outcome = ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        assert_eq!(outcome, GitignoreOutcome::Created);
        let body = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        for p in HTTUI_PATTERNS {
            assert!(body.contains(p), "missing pattern {p}: {body}");
        }
        assert!(body.contains(HTTUI_BLOCK_HEADER));
    }

    #[test]
    fn appends_to_existing_gitignore() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".gitignore");
        std::fs::write(&path, "node_modules\n.env\n").unwrap();
        let outcome = ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        assert_eq!(outcome, GitignoreOutcome::Augmented);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("node_modules\n"));
        assert!(body.contains("envs/*.local.toml"));
    }

    #[test]
    fn idempotent_when_block_already_present() {
        let dir = TempDir::new().unwrap();
        ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        let outcome = ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        assert_eq!(outcome, GitignoreOutcome::AlreadyPresent);
    }

    #[test]
    fn detects_partial_block_and_re_appends() {
        // Only one pattern present — we still re-append the whole
        // block so the rest gets in. The duplicate of the present
        // pattern is fine; gitignore is forgiving about duplicates.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".gitignore");
        std::fs::write(&path, "envs/*.local.toml\n").unwrap();
        let outcome = ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        assert_eq!(outcome, GitignoreOutcome::Augmented);
        let body = std::fs::read_to_string(&path).unwrap();
        // Now contains the full set.
        for p in HTTUI_PATTERNS {
            assert!(body.contains(p));
        }
    }

    #[test]
    fn handles_gitignore_without_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".gitignore");
        std::fs::write(&path, "node_modules").unwrap();
        ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        // Should have an empty line between existing content and our
        // block, and existing content kept intact.
        assert!(body.starts_with("node_modules\n"));
        assert!(body.contains(HTTUI_BLOCK_HEADER));
    }

    #[test]
    fn handles_empty_gitignore() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".gitignore");
        std::fs::write(&path, "").unwrap();
        let outcome = ensure_local_overrides_in_gitignore(dir.path()).unwrap();
        assert_eq!(outcome, GitignoreOutcome::Augmented);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(HTTUI_BLOCK_HEADER));
    }

    #[test]
    fn line_present_matches_exact_line() {
        assert!(line_present("a\nb\nc\n", "b"));
        assert!(line_present("  b  \n", "b"), "trims whitespace");
        assert!(!line_present("ab\n", "b"));
        assert!(!line_present("b ext\n", "b"));
    }

    #[test]
    fn gitignore_path_returns_vault_relative() {
        let p = gitignore_path(Path::new("/x/y"));
        assert_eq!(p, PathBuf::from("/x/y/.gitignore"));
    }
}
