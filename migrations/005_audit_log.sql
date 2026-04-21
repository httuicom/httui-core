-- T30: Query audit logging for incident investigation
CREATE TABLE IF NOT EXISTS query_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    connection_id TEXT,
    query TEXT NOT NULL,
    status TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_query_log_connection ON query_log(connection_id);
CREATE INDEX IF NOT EXISTS idx_query_log_created ON query_log(created_at);

-- T38: Execution locks to prevent TOCTOU race on concurrent block execution
CREATE TABLE IF NOT EXISTS block_execution_locks (
    file_path TEXT NOT NULL,
    block_hash TEXT NOT NULL,
    locked_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (file_path, block_hash)
);
