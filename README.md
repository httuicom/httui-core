# httui-core

Shared Rust platform for [httui](https://httui.com): vault management, git,
keychain integration, SQLite storage, and executors for HTTP and database
blocks.

This crate is consumed by:

- [`httui`](https://github.com/httuicom/httui) — desktop app, TUI, MCP server
- [`httui-lang`](https://github.com/httuicom/httui-lang) — language server
  (LSP wrapper that depends on httui-core for storage and execution)

## Modules

- `executor/` — HTTP and DB executors with streaming, cancellation, cookies
- `db/` — connection pools, schema cache, dialects (PostgreSQL, MySQL, SQLite)
- `git/` — git operations (clone, push, pull, conflict resolution)
- `secrets/` — OS keychain integration
- `vault_config/` — TOML config files
- `references/` — block reference parsing and resolution
- `frontmatter/`, `tag_index/`, `paths/`, and more

## Build

```bash
cargo build
cargo test
```

## License

MIT. See [LICENSE](./LICENSE).
