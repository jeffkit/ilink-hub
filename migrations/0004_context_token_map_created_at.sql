-- Replace rowid-based ordering (not portable to PostgreSQL/MySQL) with an
-- explicit created_at column so list_recent_context_tokens() sorts correctly
-- on all supported databases.
--
-- The column is nullable because SQLite forbids CURRENT_TIMESTAMP as a default
-- in ALTER TABLE ADD COLUMN (it's treated as a non-constant expression).
-- All application INSERT statements explicitly pass CURRENT_TIMESTAMP for new rows.
-- Pre-v4 rows have NULL, which list_recent_context_tokens handles via COALESCE.
ALTER TABLE context_token_map ADD COLUMN created_at TEXT;

CREATE INDEX IF NOT EXISTS idx_context_token_map_created_at
    ON context_token_map (created_at DESC);
