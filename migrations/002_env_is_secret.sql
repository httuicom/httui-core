-- Add is_secret flag to env_variables for keychain encryption
ALTER TABLE env_variables ADD COLUMN is_secret INTEGER NOT NULL DEFAULT 0