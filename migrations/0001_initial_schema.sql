-- Initial schema: clients, routing_state, context_token_map, bot_credentials

CREATE TABLE IF NOT EXISTS clients (
    vtoken      TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    label       TEXT,
    created_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
    last_seen   TEXT
);

CREATE TABLE IF NOT EXISTS routing_state (
    from_user     TEXT PRIMARY KEY,
    active_vtoken TEXT NOT NULL,
    updated_at    TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);

CREATE TABLE IF NOT EXISTS context_token_map (
    vctx         TEXT PRIMARY KEY,
    real_ctx     TEXT NOT NULL,
    peer_user_id TEXT NOT NULL DEFAULT '',
    expires_at   TEXT
);

CREATE TABLE IF NOT EXISTS bot_credentials (
    id         INTEGER PRIMARY KEY,
    token      TEXT NOT NULL,
    base_url   TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);
