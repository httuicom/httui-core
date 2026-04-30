//! Thin wrapper around the system `git` CLI for the in-app git
//! panel (Epic 20). Functions all accept a `vault_path: &Path` and
//! shell out to `git -C <vault>`. No `git2-rs` dependency — keeps
//! the build slim and the surface easy to swap to libgit2 later if
//! we need richer diff data.
//!
//! Each call returns a typed result; non-zero exit codes from `git`
//! become structured `Err(stderr)` values rather than panics, so the
//! UI can surface them.
//!
//! Network ops (`pull` / `push`) are deliberately omitted from this
//! foundation commit — they need progress reporting + auth flows
//! that will land alongside the panel UI.

pub mod log;
pub mod remote_host;
pub mod status;

pub use log::{git_log, CommitInfo};
pub use remote_host::{parse_remote_url, ParsedRemote, RemoteHost};
pub use status::{git_branch_list, git_diff, git_status, BranchInfo, GitStatus};

use std::path::Path;
use std::process::{Command, Output};

/// Run `git -C <vault> <args...>` and capture stdout. Errors carry
/// stderr verbatim so the UI can show what `git` actually said.
pub(crate) fn run_git<P: AsRef<Path>>(vault: P, args: &[&str]) -> Result<String, String> {
    let output: Output = Command::new("git")
        .arg("-C")
        .arg(vault.as_ref())
        .args(args)
        .output()
        .map_err(|e| format!("git invocation failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(if stderr.trim().is_empty() {
            format!(
                "git exited with code {}",
                output.status.code().unwrap_or(-1)
            )
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::path::Path;
    use std::process::Command;

    /// Initialise a temporary git repo at `path`, configure
    /// non-interactive identity, return the path. Caller keeps the
    /// `TempDir` alive.
    pub fn init_repo(path: &Path) {
        let init = Command::new("git").arg("init").arg(path).output().unwrap();
        assert!(init.status.success(), "git init failed");
        for (k, v) in [
            ("user.email", "test@httui.local"),
            ("user.name", "Test"),
            ("commit.gpgsign", "false"),
            ("init.defaultBranch", "main"),
        ] {
            let r = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["config", k, v])
                .output()
                .unwrap();
            assert!(r.status.success(), "git config {k} failed");
        }
    }

    /// Stage and commit everything currently in the working tree.
    /// Returns the resulting commit's full SHA.
    pub fn commit_all(path: &Path, message: &str) -> String {
        let add = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "-A"])
            .output()
            .unwrap();
        assert!(add.status.success(), "git add failed");
        let cm = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["commit", "-m", message])
            .output()
            .unwrap();
        assert!(
            cm.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&cm.stderr)
        );
        let rev = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(rev.status.success(), "git rev-parse failed");
        String::from_utf8_lossy(&rev.stdout).trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn run_git_returns_error_on_non_repo() {
        let dir = TempDir::new().unwrap();
        let err = run_git(dir.path(), &["status"]).unwrap_err();
        assert!(
            err.contains("not a git repository") || err.contains("fatal"),
            "got: {err}"
        );
    }

    #[test]
    fn run_git_returns_stdout_on_success() {
        let dir = TempDir::new().unwrap();
        test_helpers::init_repo(dir.path());
        let out = run_git(dir.path(), &["rev-parse", "--is-inside-work-tree"]).unwrap();
        assert_eq!(out.trim(), "true");
    }
}
