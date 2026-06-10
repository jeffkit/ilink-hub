-- Replace rowid-based ordering (not portable to PostgreSQL/MySQL) with an
-- explicit created_at column so list_recent_context_tokens() sorts correctly
-- on all supported databases.

ALTER TABLE context_token_map ADD COLUMN created_at TEXT NOT NULL DEFAULT (datetime('now'));

CREATE INDEX IF NOT EXISTS idx_context_token_map_created_at
    ON context_token_map (created_at DESC);
