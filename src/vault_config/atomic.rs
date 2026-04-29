//! Atomic file write helper. Filled in story 03.

use std::path::Path;

/// Write `content` to `path` atomically (write to a sibling temp file,
/// then rename). Stub for story 01 — full implementation lands in
/// story 03.
pub fn write_atomic(_path: &Path, _content: &str) -> std::io::Result<()> {
    unimplemented!("write_atomic — see story 03")
}
