-- Tool permission rules (persisted decisions)
CREATE TABLE IF NOT EXISTS tool_permissions (
    id INTEGER PRIMARY KEY,
    tool_name TEXT NOT NULL,
    path_pattern TEXT,
    workspace TEXT,
    scope TEXT NOT NULL CHECK(scope IN ('always', 'session')),
    behavior TEXT NOT NULL CHECK(behavior IN ('allow', 'deny')),
    session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_tool_permissions_lookup
    ON tool_permissions(tool_name, workspace);

-- Add cache_read_tokens column to messages (for usage tracking)
ALTER TABLE messages ADD COLUMN cache_read_tokens INTEGER;
