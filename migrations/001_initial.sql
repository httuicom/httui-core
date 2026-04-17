-- App configuration (key-value store)
CREATE TABLE IF NOT EXISTS app_config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Database connections
CREATE TABLE IF NOT EXISTS connections (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL UNIQUE,
    driver           TEXT NOT NULL CHECK (driver IN ('postgres', 'mysql', 'sqlite')),
    host             TEXT,
    port             INTEGER,
    database_name    TEXT,
    username         TEXT,
    password         TEXT,
    ssl_mode         TEXT DEFAULT 'disable' CHECK (ssl_mode IN ('disable', 'require', 'verify-ca', 'verify-full')),
    timeout_ms       INTEGER DEFAULT 10000,
    query_timeout_ms INTEGER DEFAULT 30000,
    ttl_seconds      INTEGER DEFAULT 300,
    max_pool_size    INTEGER DEFAULT 5,
    last_tested_at   TEXT,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Environments (groups of variables)
CREATE TABLE IF NOT EXISTS environments (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    is_active  INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Environment variables
CREATE TABLE IF NOT EXISTS env_variables (
    id             TEXT PRIMARY KEY,
    environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    key            TEXT NOT NULL,
    value          TEXT NOT NULL,
    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(environment_id, key)
);

-- Block execution results cache
CREATE TABLE IF NOT EXISTS block_results (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path   TEXT NOT NULL,
    block_hash  TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('success', 'error')),
    response    TEXT NOT NULL,
    total_rows  INTEGER,
    executed_at TEXT NOT NULL DEFAULT (datetime('now')),
    elapsed_ms  INTEGER NOT NULL,
    UNIQUE(file_path, block_hash)
);

-- Schema cache for SQL autocomplete
CREATE TABLE IF NOT EXISTS schema_cache (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
    table_name    TEXT NOT NULL,
    column_name   TEXT NOT NULL,
    data_type     TEXT,
    cached_at     TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(connection_id, table_name, column_name)
);

-- Full-text search index
CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(
    file_path,
    title,
    content
);
