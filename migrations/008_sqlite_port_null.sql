-- Heal records damaged by the pre-stage-8 bug in `update_connection`
-- that coerced a SQLite connection's NULL port into 0, which then
-- failed `validate_pool_config` with "port must be between 1 and 65535".

UPDATE connections SET port = NULL WHERE driver = 'sqlite' AND port = 0;
