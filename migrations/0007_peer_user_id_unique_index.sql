-- Partial unique index on context_token_map.peer_user_id (non-empty rows only).
--
-- Makes find_or_create_vctx race-free on multi-connection pools (PostgreSQL)
-- by enabling a single-statement INSERT ... ON CONFLICT (peer_user_id) DO UPDATE
-- upsert instead of the two-step SELECT + INSERT that had a TOCTOU window.
--
-- NOTE: This file documents the SQLite / PostgreSQL form. Both support the
-- WHERE clause on partial indexes. MySQL does NOT support partial indexes, so
-- the Rust migrator (migrate_to_v7_tx) skips this DDL on MySQL and falls back
-- to the serialised single-connection write pool to prevent races.
--
-- De-duplication (removing historical duplicate peer_user_id rows) is also
-- handled in the Rust migrator before this index is created, using a
-- driver-specific DELETE (SQLite uses rowid, PostgreSQL uses ctid).
-- That step cannot be expressed portably in a single SQL file.

CREATE UNIQUE INDEX IF NOT EXISTS idx_context_token_map_peer_user_id
    ON context_token_map (peer_user_id)
    WHERE peer_user_id != '';
