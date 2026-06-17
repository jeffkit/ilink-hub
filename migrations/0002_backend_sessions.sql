-- v2 session tables: per-(vctx,vtoken) session namespacing + active session pointer.
-- backend_sessions (v1) is intentionally omitted — it was superseded by backend_sessions_v2
-- before any production data accumulated and is not carried forward.

CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
    vctx               TEXT NOT NULL,
    vtoken             TEXT NOT NULL,
    session_name       TEXT NOT NULL,
    backend_session_id TEXT NOT NULL DEFAULT '',
    created_at         TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    PRIMARY KEY (vctx, vtoken, session_name)
);

CREATE TABLE IF NOT EXISTS active_sessions (
    vctx         TEXT NOT NULL,
    vtoken       TEXT NOT NULL,
    session_name TEXT NOT NULL DEFAULT 'default',
    updated_at   TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    PRIMARY KEY (vctx, vtoken)
);
