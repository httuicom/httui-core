CREATE TABLE IF NOT EXISTS block_examples (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path     TEXT NOT NULL,
    block_alias   TEXT NOT NULL,
    name          TEXT NOT NULL,
    response_json TEXT NOT NULL,
    saved_at      TEXT NOT NULL,
    UNIQUE (file_path, block_alias, name)
);

CREATE INDEX IF NOT EXISTS idx_block_examples_block
    ON block_examples (file_path, block_alias, saved_at DESC);
