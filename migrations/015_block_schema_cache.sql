-- Inferred response shapes per (file_path, alias), written by the
-- executor path on every successful run. Read-only consumers (the
-- language server) resolve `{{alias.path}}` fields against the shape.
-- Versioned: readers treat rows with a different cache_schema_version
-- as a cache miss (rebuild happens on the next run, never in place).
CREATE TABLE IF NOT EXISTS block_schema_cache (
    file_path  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    shape      TEXT NOT NULL,
    cache_schema_version INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (file_path, alias)
);
