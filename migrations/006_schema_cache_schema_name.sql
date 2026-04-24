-- Add optional schema qualifier so Postgres entries from non-public schemas
-- (and MySQL entries from the active database) can be grouped in the
-- schema panel and offered as qualified names in SQL autocomplete.

ALTER TABLE schema_cache ADD COLUMN schema_name TEXT;

-- The old UNIQUE(connection_id, table_name, column_name) is too narrow once
-- a single table name can exist in multiple schemas. Drop and recreate with
-- schema_name included. NULL is treated as a distinct value by SQLite,
-- which keeps existing SQLite entries (schema_name IS NULL) compatible.
DROP INDEX IF EXISTS sqlite_autoindex_schema_cache_1;
CREATE UNIQUE INDEX IF NOT EXISTS schema_cache_unique
    ON schema_cache(connection_id, schema_name, table_name, column_name);
