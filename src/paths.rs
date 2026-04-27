//! Filesystem layout shared across the three binaries (desktop, TUI, MCP).
//!
//! All binaries store their state in `$HOME/.config/httui/`. Older
//! installs used Tauri-namespaced directories under
//! `~/Library/Application Support/`; on first launch after upgrade,
//! [`migrate_legacy_data`] copies the most recent legacy `notes.db`
//! directory into the new location (legacy is left intact for rollback).

use std::path::{Path, PathBuf};

use crate::error::{CoreError, CoreResult};

const APP_DIR: &str = ".config/httui";

/// Legacy paths (relative to `$HOME`) that previous installs may have
/// populated. Listed in no particular order — [`migrate_legacy_data`]
/// picks the candidate with the most recent `notes.db` mtime.
const LEGACY_DIRS: &[&str] = &[
    "Library/Application Support/com.notes.app",
    "Library/Application Support/com.httui.notes",
];

fn home_dir() -> CoreResult<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| CoreError::Other("HOME is not set".into()))
}

/// Returns `$HOME/.config/httui` — the unified data directory used by
/// all binaries. The directory is *not* created here; callers that need
/// it on disk should create it themselves (or rely on
/// [`crate::db::init_db`] which does).
pub fn default_data_dir() -> CoreResult<PathBuf> {
    Ok(home_dir()?.join(APP_DIR))
}

/// Outcome of a legacy-data migration attempt.
#[derive(Debug)]
pub enum MigrationOutcome {
    /// `target` already has `notes.db` — nothing to do.
    TargetPopulated,
    /// No legacy directory contains a `notes.db` — fresh install.
    NoLegacy,
    /// Copied legacy data into `target`. The originating legacy path is
    /// returned so callers can log it; legacy is left on disk so the
    /// user can roll back if anything goes wrong.
    Migrated { from: PathBuf },
}

/// If `target` has no `notes.db` and at least one of the legacy
/// directories does, copy the legacy directory's contents into
/// `target`. Picks the legacy candidate with the most recent
/// `notes.db` mtime when several exist.
///
/// Idempotent: once `target/notes.db` exists, subsequent calls return
/// [`MigrationOutcome::TargetPopulated`] without touching anything.
pub fn migrate_legacy_data(target: &Path) -> CoreResult<MigrationOutcome> {
    let home = home_dir()?;
    let candidates: Vec<PathBuf> = LEGACY_DIRS.iter().map(|rel| home.join(rel)).collect();
    migrate_from_candidates(target, &candidates)
}

/// Inner implementation of [`migrate_legacy_data`] decoupled from
/// `$HOME` lookup so tests can inject candidate paths directly without
/// mutating the process environment (which leaks across parallel tests).
fn migrate_from_candidates(target: &Path, candidates: &[PathBuf]) -> CoreResult<MigrationOutcome> {
    if target.join("notes.db").exists() {
        return Ok(MigrationOutcome::TargetPopulated);
    }

    let source = candidates
        .iter()
        .filter(|dir| dir.join("notes.db").exists())
        .max_by_key(|dir| {
            std::fs::metadata(dir.join("notes.db"))
                .and_then(|m| m.modified())
                .ok()
        })
        .cloned();

    let Some(source) = source else {
        return Ok(MigrationOutcome::NoLegacy);
    };

    std::fs::create_dir_all(target)?;
    copy_dir_contents(&source, target)?;

    Ok(MigrationOutcome::Migrated { from: source })
}

fn copy_dir_contents(src: &Path, dst: &Path) -> CoreResult<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir_contents(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
        // symlinks and other types are skipped — none expected in our data dir
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_returns_no_legacy_when_target_and_sources_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".config/httui");
        let outcome = migrate_from_candidates(&target, &[]).unwrap();
        assert!(matches!(outcome, MigrationOutcome::NoLegacy));
    }

    #[test]
    fn migrate_skips_when_target_already_populated() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".config/httui");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("notes.db"), b"new").unwrap();

        let legacy = tmp.path().join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("notes.db"), b"old").unwrap();

        let outcome = migrate_from_candidates(&target, &[legacy]).unwrap();
        assert!(matches!(outcome, MigrationOutcome::TargetPopulated));
        // target's notes.db must be untouched
        assert_eq!(std::fs::read(target.join("notes.db")).unwrap(), b"new");
    }

    #[test]
    fn migrate_copies_single_legacy_into_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".config/httui");

        let legacy = tmp.path().join("legacy");
        std::fs::create_dir_all(legacy.join("tmp")).unwrap();
        std::fs::write(legacy.join("notes.db"), b"legacy-data").unwrap();
        std::fs::write(legacy.join("tmp/foo.png"), b"img").unwrap();

        let outcome = migrate_from_candidates(&target, &[legacy.clone()]).unwrap();
        assert!(matches!(&outcome, MigrationOutcome::Migrated { from } if from == &legacy));

        assert_eq!(std::fs::read(target.join("notes.db")).unwrap(), b"legacy-data");
        assert_eq!(std::fs::read(target.join("tmp/foo.png")).unwrap(), b"img");
        // legacy must remain intact (rollback safety)
        assert!(legacy.join("notes.db").exists());
    }

    #[test]
    fn migrate_picks_legacy_with_most_recent_notes_db() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".config/httui");

        let older = tmp.path().join("older");
        let newer = tmp.path().join("newer");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(older.join("notes.db"), b"older").unwrap();
        // ensure distinct mtimes — sleep for a tick, then write the newer
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(newer.join("notes.db"), b"newer").unwrap();

        let outcome = migrate_from_candidates(&target, &[older, newer.clone()]).unwrap();
        match outcome {
            MigrationOutcome::Migrated { from } => assert_eq!(from, newer),
            other => panic!("expected Migrated, got {other:?}"),
        }
        assert_eq!(std::fs::read(target.join("notes.db")).unwrap(), b"newer");
    }

    #[test]
    fn migrate_skips_candidates_without_notes_db() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".config/httui");

        let empty = tmp.path().join("empty"); // no notes.db
        let populated = tmp.path().join("populated");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&populated).unwrap();
        std::fs::write(populated.join("notes.db"), b"data").unwrap();

        let outcome =
            migrate_from_candidates(&target, &[empty, populated.clone()]).unwrap();
        match outcome {
            MigrationOutcome::Migrated { from } => assert_eq!(from, populated),
            other => panic!("expected Migrated, got {other:?}"),
        }
    }
}
