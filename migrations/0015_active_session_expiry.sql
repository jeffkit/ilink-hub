-- v15: bound active_sessions grants with an explicit expiry (epoch seconds).
-- Existing rows default to 0 = already expired (fail-closed): an upgrade must not
-- inherit unbounded grants. The next inbound dispatch rewrites the row (self-healing).
ALTER TABLE active_sessions ADD COLUMN expires_at BIGINT NOT NULL DEFAULT 0;
