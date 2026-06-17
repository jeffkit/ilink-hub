-- Migration version tracking table.
-- Created automatically by Store::run_migrations() before any versioned step runs.
-- Each applied migration inserts one row here; run_migrations() is idempotent:
-- it skips any version already present in this table.
--
-- NOTE: This file is documentation-only. The table is created at runtime by
-- the Rust code in src/store/mod.rs, not by running this SQL file directly.

CREATE TABLE IF NOT EXISTS schema_version (
    version     INTEGER PRIMARY KEY,
    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);
