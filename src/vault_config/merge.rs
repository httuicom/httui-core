//! Deep-merge for `*.local.toml` overrides.
//!
//! Implements the merge contract from ADR 0004:
//!
//! - Tables merge key-by-key.
//! - Leaf scalars from the override replace base values.
//! - **Arrays replace whole**, never concatenate (the only safe rule —
//!   concatenation makes deletion impossible).
//! - Keys present in the override but not in the base are added.
//!
//! Two functions live here:
//!
//! - [`deep_merge`] is the pure value-level operation, useful for tests
//!   and any caller that already has parsed `toml::Value`s.
//! - [`load_with_local`] is the file-level convenience: read base TOML
//!   text, optionally read `<base>.local.toml`, deep-merge, and
//!   deserialize into a typed struct.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

/// Deep-merge `over` into `base`. After the call, `base` holds the
/// merged result.
pub fn deep_merge(base: &mut toml::Value, over: toml::Value) {
    use toml::Value;
    match (base, over) {
        (Value::Table(base_t), Value::Table(over_t)) => {
            for (k, v) in over_t {
                match base_t.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        base_t.insert(k, v);
                    }
                }
            }
        }
        // Arrays replace; leaf scalars replace.
        (slot, over_value) => {
            *slot = over_value;
        }
    }
}

/// Build the sibling override path. Given `connections.toml` returns
/// `connections.local.toml`. Given `envs/staging.toml` returns
/// `envs/staging.local.toml`. Returns `None` if `base` has no
/// extension or no file stem.
pub fn local_override_path(base: &Path) -> Option<PathBuf> {
    let stem = base.file_stem()?.to_str()?;
    let ext = base.extension()?.to_str()?;
    let parent = base.parent();
    let name = format!("{stem}.local.{ext}");
    Some(match parent {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    })
}

/// Read the base TOML text plus its optional `*.local.toml` sibling,
/// deep-merge, and deserialize. The base is treated as empty (`{}`)
/// when the file is missing — callers handle "no base" via their own
/// `Default::default()` if they need typed defaults instead of an
/// empty table.
///
/// Returns the parsed `T` plus the override path (always returned, so
/// callers can mtime-key their cache regardless of whether the file
/// currently exists).
pub fn load_with_local<T: DeserializeOwned>(base: &Path) -> Result<(T, PathBuf), String> {
    let local = local_override_path(base)
        .ok_or_else(|| format!("invalid base path: {}", base.display()))?;

    let mut merged: toml::Value = if base.exists() {
        let text =
            std::fs::read_to_string(base).map_err(|e| format!("read {}: {e}", base.display()))?;
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", base.display()))?
    } else {
        toml::Value::Table(Default::default())
    };

    if local.exists() {
        let text = std::fs::read_to_string(&local)
            .map_err(|e| format!("read {}: {e}", local.display()))?;
        let over: toml::Value =
            toml::from_str(&text).map_err(|e| format!("parse {}: {e}", local.display()))?;
        deep_merge(&mut merged, over);
    }

    let value: T = T::deserialize(merged)
        .map_err(|e| format!("deserialize merged {}: {e}", base.display()))?;
    Ok((value, local))
}

/// `mtime` for `path`, or `None` if the file is absent or unreadable.
/// Stores use this to key their cache on `(base_mtime, local_mtime)`
/// so an external edit to either side invalidates correctly.
pub fn mtime_or_none(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn t(v: &str) -> toml::Value {
        toml::from_str(v).unwrap()
    }

    #[test]
    fn override_scalar_wins() {
        let mut base = t(r#"x = 1"#);
        deep_merge(&mut base, t(r#"x = 2"#));
        assert_eq!(base, t(r#"x = 2"#));
    }

    #[test]
    fn key_only_in_override_is_added() {
        let mut base = t(r#"x = 1"#);
        deep_merge(&mut base, t(r#"y = 2"#));
        assert_eq!(base, t("x = 1\ny = 2\n"));
    }

    #[test]
    fn key_only_in_base_survives() {
        let mut base = t("x = 1\ny = 2\n");
        deep_merge(&mut base, t(r#"x = 9"#));
        assert_eq!(base, t("x = 9\ny = 2\n"));
    }

    #[test]
    fn nested_tables_merge_key_by_key() {
        let mut base = t(r#"
[a]
x = 1
y = 2
[a.b]
inner = "base"
"#);
        let over = t(r#"
[a]
y = 99
[a.b]
inner = "local"
extra = true
"#);
        deep_merge(&mut base, over);
        assert_eq!(
            base,
            t(r#"
[a]
x = 1
y = 99
[a.b]
inner = "local"
extra = true
"#)
        );
    }

    #[test]
    fn arrays_replace_do_not_concatenate() {
        let mut base = t(r#"items = [1, 2, 3]"#);
        deep_merge(&mut base, t(r#"items = [9]"#));
        assert_eq!(base, t(r#"items = [9]"#));
    }

    #[test]
    fn array_can_be_emptied_via_override() {
        let mut base = t(r#"items = [1, 2, 3]"#);
        deep_merge(&mut base, t(r#"items = []"#));
        assert_eq!(base, t(r#"items = []"#));
    }

    #[test]
    fn empty_override_table_is_noop() {
        let mut base = t("x = 1\n[a]\nb = 2\n");
        deep_merge(&mut base, t(""));
        assert_eq!(base, t("x = 1\n[a]\nb = 2\n"));
    }

    #[test]
    fn override_replaces_table_with_scalar_when_types_differ() {
        // ADR 0004 says leaf scalars replace; the same applies when
        // the override changes the *shape* (table -> scalar). Treat
        // as full replacement for that subtree.
        let mut base = t("[a]\nx = 1\n");
        deep_merge(&mut base, t(r#"a = "string""#));
        assert_eq!(base, t(r#"a = "string""#));
    }

    #[test]
    fn local_override_path_appends_local_segment() {
        assert_eq!(
            local_override_path(Path::new("connections.toml")).unwrap(),
            PathBuf::from("connections.local.toml")
        );
        assert_eq!(
            local_override_path(Path::new("envs/staging.toml")).unwrap(),
            PathBuf::from("envs/staging.local.toml")
        );
        assert_eq!(
            local_override_path(Path::new("/abs/.httui/workspace.toml")).unwrap(),
            PathBuf::from("/abs/.httui/workspace.local.toml")
        );
    }

    #[test]
    fn local_override_path_rejects_pathological() {
        // No extension.
        assert!(local_override_path(Path::new("noext")).is_none());
        // Empty.
        assert!(local_override_path(Path::new("")).is_none());
    }

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Demo {
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        vars: BTreeMap<String, String>,
    }

    #[test]
    fn load_with_local_no_base_no_override_yields_default() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("missing.toml");
        let (d, _local): (Demo, _) = load_with_local(&base).unwrap();
        assert!(d.version.is_none());
        assert!(d.vars.is_empty());
    }

    #[test]
    fn load_with_local_base_only() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("envs.toml");
        std::fs::write(&base, "version = \"1\"\n[vars]\nA = \"1\"\n").unwrap();
        let (d, _local): (Demo, _) = load_with_local(&base).unwrap();
        assert_eq!(d.version.as_deref(), Some("1"));
        assert_eq!(d.vars.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn load_with_local_override_wins() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("envs.toml");
        std::fs::write(
            &base,
            "version = \"1\"\n[vars]\nA = \"base\"\nB = \"keep\"\n",
        )
        .unwrap();
        let local = dir.path().join("envs.local.toml");
        std::fs::write(&local, "[vars]\nA = \"override\"\nC = \"new\"\n").unwrap();

        let (d, returned_local): (Demo, _) = load_with_local(&base).unwrap();
        assert_eq!(d.vars.get("A").unwrap(), "override");
        assert_eq!(d.vars.get("B").unwrap(), "keep");
        assert_eq!(d.vars.get("C").unwrap(), "new");
        assert_eq!(returned_local, local);
    }

    #[test]
    fn load_with_local_returns_local_path_even_when_missing() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("connections.toml");
        std::fs::write(&base, "version = \"1\"\n").unwrap();
        let (_d, local): (Demo, _) = load_with_local(&base).unwrap();
        assert_eq!(local, dir.path().join("connections.local.toml"));
        assert!(!local.exists());
    }

    #[test]
    fn load_with_local_invalid_base_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("bad.toml");
        std::fs::write(&base, "this = = invalid").unwrap();
        let err: String = load_with_local::<Demo>(&base).unwrap_err();
        assert!(err.contains("parse"), "got {err}");
    }

    #[test]
    fn load_with_local_invalid_override_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("ok.toml");
        std::fs::write(&base, "version = \"1\"\n").unwrap();
        let local = dir.path().join("ok.local.toml");
        std::fs::write(&local, "this = = invalid").unwrap();
        let err = load_with_local::<Demo>(&base).unwrap_err();
        assert!(err.contains("parse"), "got {err}");
    }

    #[test]
    fn mtime_or_none_returns_some_for_existing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("x");
        std::fs::write(&p, "x").unwrap();
        assert!(mtime_or_none(&p).is_some());
    }

    #[test]
    fn mtime_or_none_returns_none_for_missing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("missing");
        assert!(mtime_or_none(&p).is_none());
    }
}
