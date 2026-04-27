//! Parity runner for the Rust side of the block parser/serializer.
//!
//! Loads every fixture under `tests/parity-fixtures/blocks/<name>/`,
//! parses the `input.md`, and asserts the result matches
//! `expected.json` exactly. The TS counterpart at
//! `src/lib/blocks/__tests__/parity.test.ts` reads the same fixtures —
//! together they guarantee the two implementations agree on every
//! published case.
//!
//! Round-trip is also covered: parsing `input.md` and re-serializing
//! must yield markdown that re-parses to the same expected shape
//! (idempotency contract).
//!
//! When this runner fails:
//! - If the fixture is wrong (rare), update `expected.json` AND make
//!   the TS runner pass too — the fixture is shared.
//! - If the parser is wrong, fix the parser. Don't tweak the fixture
//!   to mask the bug.

use std::fs;
use std::path::{Path, PathBuf};

use httui_core::blocks::{parse_blocks, serialize_block, ParsedBlock};
use serde_json::{json, Value};

/// Walk the fixtures dir, run a closure against each `(name, input,
/// expected)` triple. Skips entries that don't carry both files —
/// stub directories used for in-flight fixtures stay silent until
/// they have an `expected.json`.
fn for_each_fixture<F: FnMut(&str, &str, &Value)>(mut f: F) {
    let root = fixtures_root();
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read parity-fixtures dir {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let input_path = dir.join("input.md");
        let expected_path = dir.join("expected.json");
        if !input_path.exists() || !expected_path.exists() {
            continue;
        }
        let input = fs::read_to_string(&input_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", input_path.display()));
        let expected_raw = fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", expected_path.display()));
        let expected: Value = serde_json::from_str(&expected_raw)
            .unwrap_or_else(|e| panic!("parse JSON {}: {e}", expected_path.display()));
        f(name, &input, &expected);
    }
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("parity-fixtures")
        .join("blocks")
}

/// Render a [`ParsedBlock`] as the canonical JSON shape the fixtures
/// declare. Field order matches the README so diffs read top-down.
fn parsed_block_to_canonical(b: &ParsedBlock) -> Value {
    json!({
        "block_type": b.block_type,
        "alias": b.alias,
        "display_mode": b.display_mode,
        "params": b.params,
    })
}

/// Wrap a list of parsed blocks in the `{"blocks": [...]}` envelope
/// the fixtures use, so a doc-level fixture can describe many blocks.
fn parsed_to_envelope(parsed: &[ParsedBlock]) -> Value {
    let arr: Vec<Value> = parsed.iter().map(parsed_block_to_canonical).collect();
    json!({ "blocks": arr })
}

#[test]
fn parses_every_fixture_to_expected_shape() {
    let mut count = 0;
    for_each_fixture(|name, input, expected| {
        count += 1;
        let parsed = parse_blocks(input);
        let actual = parsed_to_envelope(&parsed);
        assert_eq!(
            &actual, expected,
            "fixture `{name}` parsed shape diverged from expected.json"
        );
    });
    assert!(count > 0, "no fixtures found — did you delete the dir?");
}

#[test]
fn serialized_output_round_trips_to_same_shape() {
    let mut count = 0;
    for_each_fixture(|name, input, expected| {
        let parsed = parse_blocks(input);
        // Re-serialize each block and re-parse the result — the
        // serializer is canonical, so a second round must converge
        // to the same parsed shape.
        let mut reserialized = String::new();
        for (i, b) in parsed.iter().enumerate() {
            if i > 0 {
                reserialized.push_str("\n\n");
            }
            reserialized.push_str(&serialize_block(b));
            reserialized.push('\n');
        }
        let reparsed = parse_blocks(&reserialized);
        let actual = parsed_to_envelope(&reparsed);
        assert_eq!(
            &actual, expected,
            "fixture `{name}` failed round-trip: parse → serialize → parse drifted from expected"
        );
        count += 1;
    });
    assert!(count > 0, "no fixtures found");
}
