-- Onda 1 — Per-block settings (HTTP block toolbar/drawer flags).
--
-- Stored separately from the fence info string so the .md file stays
-- clean. Defaults are intentionally NULL — readers fall back to per-flag
-- defaults (true for follow_redirects/verify_ssl/encode_url/trim_whitespace,
-- false for history_disabled).
--
-- Cascade: rows are purged when a block is deleted from the document or
-- the host note is removed (same wiring as block_run_history).

CREATE TABLE IF NOT EXISTS block_settings (
    file_path        TEXT NOT NULL,
    block_alias      TEXT NOT NULL,
    follow_redirects INTEGER,  -- NULL=default(true), 0=false, 1=true
    verify_ssl       INTEGER,
    encode_url       INTEGER,
    trim_whitespace  INTEGER,
    history_disabled INTEGER,
    updated_at       TEXT NOT NULL,
    PRIMARY KEY (file_path, block_alias)
);
