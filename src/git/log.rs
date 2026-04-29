//! `git log` parsing — one [`CommitInfo`] per commit.
//!
//! Uses a record-separated `--pretty=tformat:` to dodge the usual
//! "what if a commit message contains a tab" problem. Format:
//! `<sha>\x1f<short_sha>\x1f<author_name>\x1f<author_email>\x1f<unix_ts>\x1f<subject>\x1e`
//! where `\x1f` is the field separator and `\x1e` ends each record.

use std::path::Path;

use serde::Serialize;

use super::run_git;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub author_name: String,
    pub author_email: String,
    /// Author timestamp as Unix seconds. Frontend formats locally.
    pub timestamp: i64,
    pub subject: String,
}

const FS: char = '\x1f';
const RS: char = '\x1e';
const PRETTY: &str = "--pretty=tformat:%H\x1f%h\x1f%an\x1f%ae\x1f%at\x1f%s\x1e";

/// Return up to `limit` commits from `HEAD`, newest first. Pass an
/// empty `path_filter` to walk the whole tree, or a vault-relative
/// path to filter to commits touching that file/dir.
pub fn git_log(
    vault: &Path,
    limit: usize,
    path_filter: Option<&str>,
) -> Result<Vec<CommitInfo>, String> {
    let limit_arg = format!("-n{limit}");
    let mut args = vec!["log", PRETTY, &limit_arg];
    if let Some(p) = path_filter {
        args.push("--");
        args.push(p);
    }
    let raw = run_git(vault, &args)?;
    parse_log(&raw)
}

fn parse_log(raw: &str) -> Result<Vec<CommitInfo>, String> {
    let mut out = Vec::new();
    for record in raw.split(RS) {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.split(FS).collect();
        if parts.len() != 6 {
            return Err(format!(
                "malformed git log record (got {} fields, want 6): {record:?}",
                parts.len()
            ));
        }
        let timestamp: i64 = parts[4]
            .parse()
            .map_err(|e| format!("bad timestamp `{}`: {e}", parts[4]))?;
        out.push(CommitInfo {
            sha: parts[0].to_string(),
            short_sha: parts[1].to_string(),
            author_name: parts[2].to_string(),
            author_email: parts[3].to_string(),
            timestamp,
            subject: parts[5].to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{commit_all, init_repo};
    use super::*;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, name: &str, body: &str) {
        std::fs::write(path.join(name), body).unwrap();
    }

    #[test]
    fn empty_repo_returns_error() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        // No commits yet — `git log` exits non-zero.
        let err = git_log(dir.path(), 10, None).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn single_commit_round_trip() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        write(dir.path(), "README.md", "# x\n");
        let sha = commit_all(dir.path(), "first commit");
        let log = git_log(dir.path(), 10, None).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].sha, sha);
        assert_eq!(log[0].subject, "first commit");
        assert_eq!(log[0].author_email, "test@httui.local");
        assert!(log[0].timestamp > 0);
    }

    #[test]
    fn multiple_commits_newest_first() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        write(dir.path(), "a", "1");
        commit_all(dir.path(), "a");
        write(dir.path(), "b", "2");
        commit_all(dir.path(), "b");
        write(dir.path(), "c", "3");
        commit_all(dir.path(), "c");
        let log = git_log(dir.path(), 10, None).unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].subject, "c");
        assert_eq!(log[1].subject, "b");
        assert_eq!(log[2].subject, "a");
    }

    #[test]
    fn limit_caps_results() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        for i in 0..5 {
            write(dir.path(), "x", &format!("{i}"));
            commit_all(dir.path(), &format!("commit {i}"));
        }
        let log = git_log(dir.path(), 2, None).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "commit 4");
        assert_eq!(log[1].subject, "commit 3");
    }

    #[test]
    fn path_filter_restricts_to_matching_commits() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        write(dir.path(), "a", "1");
        commit_all(dir.path(), "touch a");
        write(dir.path(), "b", "1");
        commit_all(dir.path(), "touch b");
        write(dir.path(), "a", "2");
        commit_all(dir.path(), "touch a again");

        let only_a = git_log(dir.path(), 10, Some("a")).unwrap();
        assert_eq!(only_a.len(), 2);
        for c in &only_a {
            assert!(c.subject.contains("a"));
        }
    }

    #[test]
    fn parse_log_handles_subject_with_tabs_via_record_separator() {
        // Construct a synthetic record where the subject contains a
        // tab character (ASCII 0x09). The record separator is RS,
        // not tab, so the parser should keep the tab inside the
        // subject field intact.
        let synth =
            format!("deadbeef{FS}dead{FS}n{FS}e@e{FS}1700000000{FS}subject\twith\ttabs{RS}");
        let parsed = parse_log(&synth).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].subject, "subject\twith\ttabs");
    }

    #[test]
    fn parse_log_rejects_malformed_record() {
        let bad = format!("only{FS}three{FS}fields{RS}");
        let err = parse_log(&bad).unwrap_err();
        assert!(err.contains("malformed"));
    }

    #[test]
    fn parse_log_skips_empty_records() {
        // Trailing record-separator after the last commit is normal.
        let valid = format!("deadbeef{FS}dead{FS}n{FS}e@e{FS}1700000000{FS}msg{RS}\n{RS}\n");
        let parsed = parse_log(&valid).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn parse_log_rejects_bad_timestamp() {
        let bad = format!("sha{FS}sha{FS}n{FS}e@e{FS}not-a-number{FS}msg{RS}");
        let err = parse_log(&bad).unwrap_err();
        assert!(err.contains("bad timestamp"));
    }
}
