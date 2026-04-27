# Block parser/serializer parity fixtures

Shared fixtures used by **both** parsers — the Rust one in
`httui-core/src/blocks/` and the TypeScript one in `src/lib/blocks/`.

The TUI uses the Rust parser directly. The desktop frontend uses the
TS parser inside CodeMirror (because `transactionFilter` and
decorations need synchronous parses, and Tauri IPC is async). That
duplication is a known risk: a fix on one side that doesn't make it
to the other produces vault drift — files that look fine in one app
and broken in the other.

These fixtures are the safety net. Each test scenario lives in a
folder under `blocks/<name>/` with two files:

- `input.md` — raw markdown source (one or more fenced code blocks).
- `expected.json` — canonical JSON shape every parser must produce
  for the input. Fields:
  - `blocks[].block_type` (e.g. `"http"`, `"db-postgres"`)
  - `blocks[].alias` (string or null)
  - `blocks[].display_mode` (string or null — `displayMode` for http,
    `display` for db, both stored canonically as `"input"` /
    `"split"` / `"output"`)
  - `blocks[].params` — semantic parameters (URL, method, headers,
    query, body for http; query, connection, limit, timeout for db).

The Rust runner is `httui-core/tests/block_parity.rs`. The TS runner
lives next to the parser at `src/lib/blocks/__tests__/parity.test.ts`
and reads the same fixtures via a relative path. Both runners compare
their parser's output against `expected.json` — drift fails CI on
either side.

## Adding a fixture

1. Create `blocks/<name>/input.md` with a representative block.
2. Run the Rust runner once with the assertion commented out and
   inspect the parsed output to populate `expected.json`. Keep the
   field order canonical (block_type, alias, display_mode, params).
3. Re-enable the assertion and verify both runners pass.

## When parsers diverge

If a fixture passes one runner and fails the other, fix the parser
that's wrong — not the fixture. The fixture is the contract.
