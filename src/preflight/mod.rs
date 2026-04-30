//! Pre-flight checklist parsing + evaluation.
//!
//! Canvas §4 mocks a row of pills above the runbook body — one per
//! item from the YAML frontmatter `preflight:` block-list. Each
//! item declares one of six check kinds (connection / env_var /
//! branch / keychain / file_exists / command); Story 02 evaluates
//! them against vault context.
//!
//! Story 01 (this slice) ships only the parser. The parser reads
//! the raw frontmatter region produced by `crate::frontmatter` —
//! the slice-1 YAML parser keeps block-list children verbatim in
//! `raw_yaml`, so this module owns the typed extraction for the
//! `preflight:` section without touching the generic YAML parser.

pub mod parser;

pub use parser::{parse_preflight, PreflightItem};
