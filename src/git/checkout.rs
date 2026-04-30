//! `git checkout` + `git checkout -b` — branch switching.

use std::path::Path;

use super::run_git;

/// `git checkout <branch>` — switch to an existing branch. Errors
/// surface verbatim from git so the consumer can show them in a
/// toast (uncommitted changes, branch not found, etc.).
pub fn git_checkout(vault: &Path, branch: &str) -> Result<(), String> {
    if branch.trim().is_empty() {
        return Err("branch name is empty".into());
    }
    run_git(vault, &["checkout", branch])?;
    Ok(())
}

/// `git checkout -b <new>` — create a new branch from the current
/// branch and switch to it.
pub fn git_checkout_b(vault: &Path, new_branch: &str) -> Result<(), String> {
    if new_branch.trim().is_empty() {
        return Err("new branch name is empty".into());
    }
    run_git(vault, &["checkout", "-b", new_branch])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{commit_all, init_repo};
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn checkout_b_creates_and_switches() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a"), "x").unwrap();
        commit_all(dir.path(), "init");
        git_checkout_b(dir.path(), "feat/x").unwrap();
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "feat/x");
    }

    #[test]
    fn checkout_switches_back_to_main() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a"), "x").unwrap();
        commit_all(dir.path(), "init");
        git_checkout_b(dir.path(), "feat/x").unwrap();
        git_checkout(dir.path(), "main").unwrap();
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "main");
    }

    #[test]
    fn checkout_returns_error_for_unknown_branch() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a"), "x").unwrap();
        commit_all(dir.path(), "init");
        let err = git_checkout(dir.path(), "nope").unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn checkout_rejects_empty_branch_name() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let err = git_checkout(dir.path(), "  ").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn checkout_b_rejects_empty_branch_name() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let err = git_checkout_b(dir.path(), "").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn checkout_b_returns_error_when_branch_already_exists() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a"), "x").unwrap();
        commit_all(dir.path(), "init");
        git_checkout_b(dir.path(), "feat/x").unwrap();
        // Switch back so we're not on feat/x.
        git_checkout(dir.path(), "main").unwrap();
        // Try to create the same name again.
        let err = git_checkout_b(dir.path(), "feat/x").unwrap_err();
        assert!(!err.is_empty());
    }
}
