//! `.env` auto-discovery: parse + classify (Epic 54 Story 01).
//!
//! Scope: pure-string parsing and classification. The vault-side
//! scanner that walks the filesystem on vault open + the Tauri
//! command + UI banner ship in Stories 02-04 of Epic 54.

use once_cell::sync::Lazy;
use regex::Regex;

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
}
