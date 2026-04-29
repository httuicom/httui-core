//! `.env` auto-discovery: parse + classify + scan (Epic 54 Story 01).
//!
//! Pure-string parsing + classification + a vault-root scanner that
//! looks for `.env`-style files at the root and one level deep. The
//! Tauri command, banner UI, and import flow ship in Stories 02-04 of
//! Epic 54.

use once_cell::sync::Lazy;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

/// One parsed entry from a `.env`-style file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotenvEntry {
    pub key: String,
    pub value: String,
    pub kind: EntryKind,
}

/// Classification of a parsed entry — drives the import preview's
/// suggested destination (connections.toml row, env secret, env var).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// Value parses as a `scheme://...` connection URI for one of
    /// the known SQL / NoSQL drivers.
    ConnectionString { driver: String },
    /// Key name suggests a secret (token / password / api_key / …).
    Secret,
    /// Anything else — a plain string variable.
    Plain,
}

// --- Parser ------------------------------------------------------------

/// Parse a `.env`-style file. Honors `#` comments, blank lines,
/// `KEY=VALUE`, single + double-quoted values, basic backslash
/// escapes inside double quotes (`\n`, `\t`, `\"`, `\\`).
///
/// Lines that don't fit `KEY=VALUE` are silently skipped — `.env`
/// files vary in tooling and the exact tolerance differs by parser;
/// this leans permissive.
pub fn parse_dotenv(content: &str) -> Vec<DotenvEntry> {
    let mut out = Vec::new();
    for line in content.lines() {
        // Strip optional `export ` prefix (bashrc-style `.env`s use it).
        let trimmed = line.trim_start();
        let line_body = trimmed
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(trimmed);

        if line_body.is_empty() || line_body.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line_body.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        if key.is_empty() || !is_valid_key(&key) {
            continue;
        }

        let value = unquote(value.trim_start());
        let kind = classify(&key, &value);
        out.push(DotenvEntry { key, value, kind });
    }
    out
}

fn is_valid_key(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn unquote(raw: &str) -> String {
    // Strip trailing inline comments first only when the line is
    // unquoted — quoted values may contain `#`.
    if let Some(rest) = raw.strip_prefix('"') {
        if let Some(end) = find_closing_quote(rest, '"') {
            return decode_double_quoted(&rest[..end]);
        }
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        if let Some(end) = find_closing_quote(rest, '\'') {
            return rest[..end].to_string();
        }
    }
    // Unquoted: strip `# comment` tail + trim whitespace.
    let cut = raw.find(" #").map(|i| &raw[..i]).unwrap_or(raw);
    cut.trim().to_string()
}

fn find_closing_quote(s: &str, delim: char) -> Option<usize> {
    let mut iter = s.char_indices();
    while let Some((i, c)) = iter.next() {
        if c == '\\' && delim == '"' {
            // Skip the next char (escape).
            iter.next();
            continue;
        }
        if c == delim {
            return Some(i);
        }
    }
    None
}

fn decode_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars();
    while let Some(c) = iter.next() {
        if c == '\\' {
            match iter.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// --- Classifier --------------------------------------------------------

/// Connection-string scheme prefixes we recognize. A value starting
/// with one of these (case-insensitive) classifies as
/// `ConnectionString` and surfaces the canonical driver name.
const CONNECTION_SCHEMES: &[(&str, &str)] = &[
    ("postgres://", "postgres"),
    ("postgresql://", "postgres"),
    ("mysql://", "mysql"),
    ("mariadb://", "mysql"),
    ("sqlite://", "sqlite"),
    ("mongodb://", "mongo"),
    ("mongodb+srv://", "mongo"),
    ("redis://", "redis"),
    ("rediss://", "redis"),
];

static SECRET_KEY_RE: Lazy<Regex> = Lazy::new(|| {
    // Case-insensitive match against common secret-shaped key names.
    // Boundary on `_`/`-`/start/end so `STRIPE_KEY` matches but
    // `KEYBOARD_LAYOUT` doesn't.
    Regex::new(
        r"(?i)(^|[_-])(password|passwd|pwd|secret|key|token|auth|credential|apikey)([_-]|$)",
    )
    .expect("secret regex compiles")
});

/// Apply the classifier to a single (key, value) pair.
pub fn classify(key: &str, value: &str) -> EntryKind {
    let lower = value.to_lowercase();
    for (scheme, driver) in CONNECTION_SCHEMES {
        if lower.starts_with(scheme) {
            return EntryKind::ConnectionString {
                driver: (*driver).to_string(),
            };
        }
    }
    if SECRET_KEY_RE.is_match(key) {
        return EntryKind::Secret;
    }
    EntryKind::Plain
}

// --- Scanner -----------------------------------------------------------

/// Filenames recognised as `.env`-style by the auto-discovery scanner.
const DOTENV_FILENAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.example",
    ".envrc",
    "dotenv.config",
];

/// Subdirectories skipped during the first-level scan: typical noisy
/// or generated trees that almost never hold runbook-relevant secrets.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".httui",
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    ".next",
    ".cache",
];

/// One scanned file with its parsed entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotenvFile {
    pub path: PathBuf,
    pub entries: Vec<DotenvEntry>,
}

/// Scan `root` and its first-level subdirectories for `.env`-style
/// files. Returns one `DotenvFile` per file that parsed at least one
/// entry. Files that don't exist or fail to read are silently skipped
/// — auto-discovery is best-effort.
pub fn scan_dotenv_files(root: &Path) -> Vec<DotenvFile> {
    let mut out = Vec::new();
    scan_dir(root, &mut out);

    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if SKIP_DIRS.contains(&name) {
            continue;
        }
        scan_dir(&path, &mut out);
    }
    out
}

fn scan_dir(dir: &Path, out: &mut Vec<DotenvFile>) {
    for &name in DOTENV_FILENAMES {
        let path = dir.join(name);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let entries = parse_dotenv(&content);
        if !entries.is_empty() {
            out.push(DotenvFile { path, entries });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(content: &str) -> DotenvEntry {
        parse_dotenv(content).into_iter().next().expect("at least one entry")
    }

    #[test]
    fn parses_simple_assignment() {
        let e = first("HOST=localhost");
        assert_eq!(e.key, "HOST");
        assert_eq!(e.value, "localhost");
        assert!(matches!(e.kind, EntryKind::Plain));
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let entries = parse_dotenv(
            "# top comment\n\nHOST=h\n  # indented comment\nPORT=5432\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "HOST");
        assert_eq!(entries[1].key, "PORT");
    }

    #[test]
    fn handles_export_prefix() {
        let e = first("export HOST=h");
        assert_eq!(e.key, "HOST");
        assert_eq!(e.value, "h");
    }

    #[test]
    fn double_quoted_value_decodes_escapes() {
        let e = first(r#"GREETING="hello\nworld""#);
        assert_eq!(e.value, "hello\nworld");
    }

    #[test]
    fn single_quoted_value_is_literal() {
        let e = first(r#"PATH='C:\\Users\\me'"#);
        assert_eq!(e.value, r"C:\\Users\\me");
    }

    #[test]
    fn unquoted_strips_inline_comment() {
        let e = first("HOST=localhost # local dev");
        assert_eq!(e.value, "localhost");
    }

    #[test]
    fn quoted_value_keeps_hash_inside() {
        let e = first(r##"NOTE="text # not a comment""##);
        assert_eq!(e.value, "text # not a comment");
    }

    #[test]
    fn rejects_invalid_keys() {
        let entries = parse_dotenv("1BAD=x\nGOOD=y\n=alsobad\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "GOOD");
    }

    #[test]
    fn classifies_postgres_connection_string() {
        let e = first("DATABASE_URL=postgres://u:p@h:5432/db");
        assert_eq!(
            e.kind,
            EntryKind::ConnectionString {
                driver: "postgres".to_string()
            }
        );
    }

    #[test]
    fn classifies_postgresql_alias() {
        let e = first("DB=postgresql://u@h/d");
        assert_eq!(
            e.kind,
            EntryKind::ConnectionString {
                driver: "postgres".to_string()
            }
        );
    }

    #[test]
    fn classifies_mysql_mariadb_mongo_redis_sqlite() {
        for (raw, expected) in [
            ("M=mysql://u@h/d", "mysql"),
            ("M=mariadb://u@h/d", "mysql"),
            ("M=mongodb://h/d", "mongo"),
            ("M=mongodb+srv://h/d", "mongo"),
            ("M=redis://h:6379", "redis"),
            ("M=rediss://h:6380", "redis"),
            ("M=sqlite:///tmp/x.db", "sqlite"),
        ] {
            let e = first(raw);
            assert_eq!(
                e.kind,
                EntryKind::ConnectionString {
                    driver: expected.to_string()
                },
                "case: {raw}"
            );
        }
    }

    #[test]
    fn classifies_secret_keys() {
        for key in [
            "PASSWORD", "API_KEY", "APIKEY", "TOKEN",
            "STRIPE_SECRET", "MY_AUTH", "DB_PWD", "X_CREDENTIAL",
            "STRIPE_KEY",
        ] {
            let raw = format!("{key}=value-doesnt-matter");
            let e = first(&raw);
            assert_eq!(e.kind, EntryKind::Secret, "case: {key}");
        }
    }

    #[test]
    fn classifies_plain_vars() {
        for raw in ["PORT=5432", "LOG_LEVEL=info", "REGION=us-east-1"] {
            let e = first(raw);
            assert!(
                matches!(e.kind, EntryKind::Plain),
                "case: {raw} got {:?}",
                e.kind
            );
        }
    }

    #[test]
    fn connection_scheme_wins_over_secret_key() {
        // `DATABASE_URL` would match neither of our regex patterns but
        // a key named `DB_PASSWORD` whose value is a connection
        // string still classifies as ConnectionString — the value
        // shape is the strongest signal.
        let e = first("DB_PASSWORD=postgres://u:p@h/d");
        assert!(matches!(e.kind, EntryKind::ConnectionString { .. }));
    }

    #[test]
    fn parses_realworld_mix() {
        let raw = r#"
# Production env
DATABASE_URL=postgres://app:secret@db.acme.com:5432/prod
STRIPE_KEY="sk_live_abc123"
LOG_LEVEL=info
PORT=8080
# token used by webhook
WEBHOOK_TOKEN='abc-def-123'
"#;
        let entries = parse_dotenv(raw);
        assert_eq!(entries.len(), 5);
        assert!(matches!(
            entries[0].kind,
            EntryKind::ConnectionString { .. }
        ));
        assert_eq!(entries[1].kind, EntryKind::Secret);
        assert!(matches!(entries[2].kind, EntryKind::Plain));
        assert!(matches!(entries[3].kind, EntryKind::Plain));
        assert_eq!(entries[4].kind, EntryKind::Secret);
    }

    // --- Scanner tests --------------------------------------------------

    use std::fs as stdfs;
    use tempfile::tempdir;

    #[test]
    fn scan_finds_dotenv_at_root() {
        let dir = tempdir().expect("tempdir");
        stdfs::write(dir.path().join(".env"), "FOO=bar\nDB=postgres://u@h/d\n").unwrap();

        let found = scan_dotenv_files(dir.path());

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, dir.path().join(".env"));
        assert_eq!(found[0].entries.len(), 2);
    }

    #[test]
    fn scan_finds_all_known_filenames_at_root() {
        let dir = tempdir().expect("tempdir");
        for name in [
            ".env",
            ".env.local",
            ".env.development",
            ".env.example",
            ".envrc",
            "dotenv.config",
        ] {
            stdfs::write(dir.path().join(name), "K=v\n").unwrap();
        }

        let found = scan_dotenv_files(dir.path());

        assert_eq!(found.len(), 6);
    }

    #[test]
    fn scan_includes_first_level_subdirs() {
        let dir = tempdir().expect("tempdir");
        stdfs::create_dir(dir.path().join("services")).unwrap();
        stdfs::write(dir.path().join("services").join(".env"), "API=v\n").unwrap();

        let found = scan_dotenv_files(dir.path());

        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("services/.env"));
    }

    #[test]
    fn scan_skips_known_noisy_dirs() {
        let dir = tempdir().expect("tempdir");
        for noisy in ["node_modules", "target", ".git", "dist"] {
            stdfs::create_dir(dir.path().join(noisy)).unwrap();
            stdfs::write(dir.path().join(noisy).join(".env"), "X=y\n").unwrap();
        }

        let found = scan_dotenv_files(dir.path());

        assert!(found.is_empty(), "should ignore noisy dirs, got {found:?}");
    }

    #[test]
    fn scan_does_not_recurse_below_first_level() {
        let dir = tempdir().expect("tempdir");
        let deep = dir.path().join("apps").join("api");
        stdfs::create_dir_all(&deep).unwrap();
        stdfs::write(deep.join(".env"), "DEEP=1\n").unwrap();

        let found = scan_dotenv_files(dir.path());

        assert!(found.is_empty(), "second-level .env must not surface");
    }

    #[test]
    fn scan_skips_empty_or_comment_only_files() {
        let dir = tempdir().expect("tempdir");
        stdfs::write(dir.path().join(".env"), "# only comments\n\n").unwrap();

        let found = scan_dotenv_files(dir.path());

        assert!(found.is_empty());
    }

    #[test]
    fn scan_returns_empty_for_missing_root() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");

        let found = scan_dotenv_files(&missing);

        assert!(found.is_empty());
    }
}
