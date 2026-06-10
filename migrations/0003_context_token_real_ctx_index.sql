-- Unique index on context_token_map.real_ctx to enable race-free upsert
-- in map_context_token (INSERT ... ON CONFLICT (real_ctx) DO NOTHING).
-- Required for correct behaviour under concurrent Broadcast dispatch.

CREATE UNIQUE INDEX IF NOT EXISTS idx_context_token_map_real_ctx
    ON context_token_map (real_ctx);
