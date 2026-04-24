-- Mark a connection as read-only. When set, the frontend prompts the user
-- to confirm any query that starts with a mutation keyword before sending
-- it to the backend.

ALTER TABLE connections ADD COLUMN is_readonly INTEGER NOT NULL DEFAULT 0;
