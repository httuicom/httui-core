-- Local-only aggregated feature usage. One row per (day, feature) with a
-- running count — no payloads, no per-event rows, never leaves the machine.
CREATE TABLE IF NOT EXISTS feature_usage_stats (
    date TEXT NOT NULL,
    feature TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (date, feature)
);
