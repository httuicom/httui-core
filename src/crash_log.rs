//! Local crash-log storage — plain-text files under `<data_dir>/crashes`,
//! written by the desktop panic hook and the language server's top-level
//! exception handler. Nothing is ever uploaded; the desktop "Crashes"
//! settings panel reads them back so a user can inspect or clear them.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::paths::default_data_dir;

/// `<data_dir>/crashes` — the shared directory every binary writes to.
pub fn crashes_dir() -> CoreResult<PathBuf> {
    Ok(default_data_dir()?.join("crashes"))
}

/// Metadata for one crash file. The body is fetched separately via
/// [`read_crash`] so the list view stays cheap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrashLog {
    /// File name — also the id passed to [`read_crash`].
    pub name: String,
    /// Origin tag parsed from the file name (e.g. `desktop`, `lsp`).
    pub source: String,
    /// Capture time, Unix epoch milliseconds. The frontend formats it.
    pub epoch_ms: i64,
    /// First non-empty line of the body, for the list preview.
    pub summary: String,
}

/// Replace anything that isn't `[A-Za-z0-9_]` with `_` so a source tag can
/// never inject a path separator or break the `<ms>-<source>.log` shape.
fn sanitize_source(source: &str) -> String {
    source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Append a crash entry and return its file name. Best-effort and
/// panic-safe: every error is swallowed and `None` returned, because this
/// runs from a panic hook where a second failure must not abort the
/// process further.
pub fn write_crash(dir: &Path, source: &str, body: &str) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let name = format!("{ms}-{}.log", sanitize_source(source));
    std::fs::write(dir.join(&name), body).ok()?;
    Some(name)
}

/// List crash files newest-first. Files that don't match the
/// `<ms>-<source>.log` shape are skipped. Missing directory → empty list.
pub fn list_crashes(dir: &Path) -> Vec<CrashLog> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<CrashLog> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".log") else {
            continue;
        };
        let Some((ms_str, source)) = stem.split_once('-') else {
            continue;
        };
        let Ok(epoch_ms) = ms_str.parse::<i64>() else {
            continue;
        };
        let source = source.to_string();
        let summary = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|b| {
                b.lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.chars().take(200).collect::<String>())
            })
            .unwrap_or_default();
        out.push(CrashLog {
            name,
            source,
            epoch_ms,
            summary,
        });
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.epoch_ms));
    out
}

/// Read a single crash file's full body. `name` must be a bare file name
/// (no path separators or `..`) — otherwise this rejects it rather than
/// letting a caller escape the crashes directory.
pub fn read_crash(dir: &Path, name: &str) -> CoreResult<String> {
    if Path::new(name).file_name().map(|f| f.to_string_lossy()) != Some(name.into()) {
        return Err(CoreError::Other("invalid crash log name".into()));
    }
    std::fs::read_to_string(dir.join(name))
        .map_err(|e| CoreError::Other(format!("failed to read crash log: {e}")))
}

/// Delete every `.log` file in the crashes directory. Missing directory is
/// a no-op success.
pub fn clear_crashes(dir: &Path) -> CoreResult<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::Other(format!("failed to read crashes dir: {e}"))),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) == Some("log") {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_list_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("crashes");
        let name = write_crash(&dir, "desktop", "boom\nat line 1").unwrap();
        assert!(name.ends_with("-desktop.log"));

        let logs = list_crashes(&dir);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].source, "desktop");
        assert_eq!(logs[0].summary, "boom");
        assert!(logs[0].epoch_ms > 0);
        assert_eq!(read_crash(&dir, &logs[0].name).unwrap(), "boom\nat line 1");
    }

    #[test]
    fn list_is_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("crashes");
        std::fs::create_dir_all(&dir).unwrap();
        // Write files with explicit, controlled timestamps.
        std::fs::write(dir.join("100-lsp.log"), "old").unwrap();
        std::fs::write(dir.join("200-desktop.log"), "new").unwrap();

        let logs = list_crashes(&dir);
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].epoch_ms, 200);
        assert_eq!(logs[1].epoch_ms, 100);
    }

    #[test]
    fn list_skips_malformed_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("crashes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("no-number-here.log"), "x").unwrap();
        std::fs::write(dir.join("300-ok.log"), "x").unwrap();

        let logs = list_crashes(&dir);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].source, "ok");
    }

    #[test]
    fn list_on_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_crashes(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn read_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("crashes");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_crash(&dir, "../secret").is_err());
        assert!(read_crash(&dir, "sub/dir.log").is_err());
    }

    #[test]
    fn sanitize_source_strips_separators() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("crashes");
        let name = write_crash(&dir, "a/b ../c", "x").unwrap();
        // No separators survive — the file lives directly in `dir`.
        assert!(name.ends_with("-a_b____c.log"));
        assert!(dir.join(&name).is_file());
    }

    #[test]
    fn clear_removes_only_log_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("crashes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("100-desktop.log"), "x").unwrap();
        std::fs::write(dir.join("keep.txt"), "x").unwrap();

        clear_crashes(&dir).unwrap();
        assert!(list_crashes(&dir).is_empty());
        assert!(dir.join("keep.txt").exists());
    }

    #[test]
    fn clear_on_missing_dir_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(clear_crashes(&tmp.path().join("nope")).is_ok());
    }
}
