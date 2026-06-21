-- Add composite index to speed up quote fallback lookup by prefix scan.
-- Covers: peer_user_id equality + role filter + latest-row selection via id DESC.
CREATE INDEX IF NOT EXISTS idx_messages_lookup
    ON messages (peer_user_id, role, id DESC);
