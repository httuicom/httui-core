-- Chat sessions
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    claude_session_id TEXT,
    title TEXT NOT NULL DEFAULT 'Nova conversa',
    cwd TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    archived_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);

-- Chat messages
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK(role IN ('user', 'assistant')),
    turn_index INTEGER NOT NULL,
    content_json TEXT NOT NULL,
    tokens_in INTEGER,
    tokens_out INTEGER,
    is_partial INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, turn_index);

-- Attachments (files on disk, not bytes in DB)
CREATE TABLE IF NOT EXISTS attachments (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('image', 'document')),
    media_type TEXT NOT NULL,
    path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Tool call records for UI reconstruction
CREATE TABLE IF NOT EXISTS tool_calls (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    tool_use_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    input_json TEXT NOT NULL,
    result_json TEXT,
    is_error INTEGER DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Token usage aggregation
CREATE TABLE IF NOT EXISTS usage_stats (
    date TEXT NOT NULL,
    session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (date, session_id)
);
