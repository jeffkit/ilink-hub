//! Database persistence layer.
//! Uses sqlx with runtime driver selection via `DATABASE_URL`:
//!   sqlite:~/.ilink-hub/ilink-hub.db → SQLite (default, file created if missing)
//!   postgres://user:pass@host/db      → PostgreSQL
//!   mysql://user:pass@host/db         → MySQL

use anyhow::Result;
use sqlx::{AnyPool, Row};
use uuid::Uuid;

pub struct Store {
    pool: AnyPool,
}

impl Store {
    /// Connect to the database and run migrations.
    ///
    /// For SQLite URLs, the database file is created automatically if it does
    /// not exist yet (equivalent to `create_if_missing(true)`).
    pub async fn connect(url: &str) -> Result<Self> {
        sqlx::any::install_default_drivers();

        // For SQLite we must ensure the file (and its parent directory) exist
        // before connecting, because sqlx's AnyPool does not set
        // `create_if_missing` by default and will return SQLITE_CANTOPEN (14).
        if url.starts_with("sqlite:") {
            Self::ensure_sqlite_file(url)?;
        }

        let pool = AnyPool::connect(url).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Extract the file path from a SQLite URL and create the file + parent
    /// directories if they do not already exist.
    fn ensure_sqlite_file(url: &str) -> Result<()> {
        // Strip the "sqlite:" scheme prefix; handle the optional // or ///
        let path_part = url
            .strip_prefix("sqlite:///")
            .or_else(|| url.strip_prefix("sqlite://"))
            .or_else(|| url.strip_prefix("sqlite:"))
            .unwrap_or("");

        // Drop any query string (e.g. "?mode=rwc")
        let path_str = path_part.split('?').next().unwrap_or("").trim();

        // Skip in-memory databases (:memory: or empty)
        if path_str.is_empty() || path_str == ":memory:" {
            return Ok(());
        }

        let path = std::path::Path::new(path_str);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        if !path.exists() {
            std::fs::File::create(path)?;
        }
        Ok(())
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS clients (
                vtoken       TEXT PRIMARY KEY,
                name         TEXT NOT NULL UNIQUE,
                label        TEXT,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_seen    TIMESTAMPTZ
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS routing_state (
                from_user        TEXT PRIMARY KEY,
                active_vtoken    TEXT NOT NULL,
                updated_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS context_token_map (
                vctx        TEXT PRIMARY KEY,
                real_ctx    TEXT NOT NULL,
                expires_at  TIMESTAMPTZ
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Soft migration: add peer_user_id if not present (idempotent).
        let _ = sqlx::query(
            "ALTER TABLE context_token_map ADD COLUMN peer_user_id TEXT NOT NULL DEFAULT ''",
        )
        .execute(&self.pool)
        .await;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS bot_credentials (
                id        INTEGER PRIMARY KEY,
                token     TEXT NOT NULL,
                base_url  TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Soft migration: add active_session_name to context_token_map if not present (idempotent).
        let _ = sqlx::query(
            "ALTER TABLE context_token_map ADD COLUMN active_session_name TEXT NOT NULL DEFAULT 'default'",
        )
        .execute(&self.pool)
        .await;

        // Soft migration: add unique index on real_ctx to support race-free upsert in
        // map_context_token. Idempotent — CREATE UNIQUE INDEX IF NOT EXISTS is safe to re-run.
        let _ = sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_context_token_map_real_ctx \
             ON context_token_map (real_ctx)",
        )
        .execute(&self.pool)
        .await;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS backend_sessions (
                vctx             TEXT NOT NULL,
                session_name     TEXT NOT NULL,
                backend_session_id TEXT NOT NULL DEFAULT '',
                created_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (vctx, session_name)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // v2: sessions are scoped per (vctx, vtoken), so each backend has its own
        // independent session namespace for the same WeChat conversation.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                vctx               TEXT NOT NULL,
                vtoken             TEXT NOT NULL,
                session_name       TEXT NOT NULL,
                backend_session_id TEXT NOT NULL DEFAULT '',
                created_at         TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (vctx, vtoken, session_name)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Active session pointer per (vctx, vtoken) — which named session is currently
        // selected for each (user, backend) pair.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS active_sessions (
                vctx         TEXT NOT NULL,
                vtoken       TEXT NOT NULL,
                session_name TEXT NOT NULL DEFAULT 'default',
                updated_at   TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (vctx, vtoken)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ─── Clients ─────────────────────────────────────────────────────────────

    pub async fn upsert_client(&self, vtoken: &str, name: &str, label: Option<&str>) -> Result<()> {
        // ON CONFLICT (name): update vtoken so a post-restart re-registration with a new
        // vtoken wins, keeping DB and in-memory registry consistent.
        sqlx::query(
            r#"
            INSERT INTO clients (vtoken, name, label)
            VALUES ($1, $2, $3)
            ON CONFLICT (name) DO UPDATE
              SET vtoken = EXCLUDED.vtoken,
                  label = EXCLUDED.label,
                  last_seen = CURRENT_TIMESTAMP
            "#,
        )
        .bind(vtoken)
        .bind(name)
        .bind(label)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn touch_client(&self, vtoken: &str) -> Result<()> {
        sqlx::query("UPDATE clients SET last_seen = CURRENT_TIMESTAMP WHERE vtoken = $1")
            .bind(vtoken)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientRow>> {
        let rows = sqlx::query("SELECT vtoken, name, label, last_seen FROM clients ORDER BY name")
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|r| ClientRow {
                vtoken: r.get("vtoken"),
                name: r.get("name"),
                label: r.get("label"),
                last_seen: r.get::<Option<String>, _>("last_seen"),
            })
            .collect())
    }

    pub async fn get_client_by_name(&self, name: &str) -> Result<Option<ClientRow>> {
        let row = sqlx::query("SELECT vtoken, name, label, last_seen FROM clients WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| ClientRow {
            vtoken: r.get("vtoken"),
            name: r.get("name"),
            label: r.get("label"),
            last_seen: r.get::<Option<String>, _>("last_seen"),
        }))
    }

    pub async fn delete_client_by_name(&self, name: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM clients WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_client_by_vtoken(
        &self,
        vtoken: &str,
        name: &str,
        label: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE clients SET name = $2, label = $3 WHERE vtoken = $1")
            .bind(vtoken)
            .bind(name)
            .bind(label)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear_routes_for_vtoken(&self, vtoken: &str) -> Result<()> {
        sqlx::query("DELETE FROM routing_state WHERE active_vtoken = $1")
            .bind(vtoken)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ─── Routing state ────────────────────────────────────────────────────────

    pub async fn list_routes(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT from_user, active_vtoken FROM routing_state")
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("from_user"),
                    r.get::<String, _>("active_vtoken"),
                )
            })
            .collect())
    }

    pub async fn get_route(&self, from_user: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT active_vtoken FROM routing_state WHERE from_user = $1")
            .bind(from_user)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("active_vtoken")))
    }

    pub async fn set_route(&self, from_user: &str, vtoken: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO routing_state (from_user, active_vtoken)
            VALUES ($1, $2)
            ON CONFLICT (from_user) DO UPDATE
              SET active_vtoken = EXCLUDED.active_vtoken,
                  updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(from_user)
        .bind(vtoken)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ─── Backend sessions (named sessions per virtual context + backend token) ──

    /// Upsert the backend session UUID for a named session scoped to a specific backend.
    pub async fn set_backend_session(
        &self,
        vctx: &str,
        vtoken: &str,
        session_name: &str,
        backend_session_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO backend_sessions_v2 (vctx, vtoken, session_name, backend_session_id)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (vctx, vtoken, session_name) DO UPDATE SET
                backend_session_id = excluded.backend_session_id
            "#,
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .bind(backend_session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get the backend session UUID for a named session scoped to a specific backend.
    pub async fn get_backend_session(
        &self,
        vctx: &str,
        vtoken: &str,
        session_name: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT backend_session_id FROM backend_sessions_v2 \
             WHERE vctx = $1 AND vtoken = $2 AND session_name = $3",
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<String, _>("backend_session_id")))
    }

    /// List all named sessions for a (vctx, vtoken) pair — i.e. for one user × one backend.
    pub async fn list_backend_sessions(
        &self,
        vctx: &str,
        vtoken: &str,
    ) -> Result<Vec<BackendSessionRow>> {
        let rows = sqlx::query(
            "SELECT session_name, backend_session_id FROM backend_sessions_v2 \
             WHERE vctx = $1 AND vtoken = $2 ORDER BY session_name",
        )
        .bind(vctx)
        .bind(vtoken)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| BackendSessionRow {
                session_name: r.get("session_name"),
                backend_session_id: r.get("backend_session_id"),
            })
            .collect())
    }

    /// Delete a named session for a specific (vctx, vtoken) pair.
    pub async fn delete_backend_session(
        &self,
        vctx: &str,
        vtoken: &str,
        session_name: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM backend_sessions_v2 WHERE vctx = $1 AND vtoken = $2 AND session_name = $3",
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Batch-fetch HubExt data for multiple (vctx, vtoken) pairs in two queries.
    ///
    /// Returns a map of `(vctx, vtoken) → (session_name, Option<backend_session_id>)`.
    /// Used by the Broadcast path to avoid N×2 individual DB round-trips.
    pub async fn get_hub_ext_batch(
        &self,
        pairs: &[(String, String)], // (vctx, vtoken)
    ) -> Result<std::collections::HashMap<(String, String), (String, Option<String>)>> {
        if pairs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Build placeholders: ($1,$2), ($3,$4), ...
        let mut active_placeholders = Vec::with_capacity(pairs.len());
        for i in 0..pairs.len() {
            active_placeholders.push(format!("(${}, ${})", i * 2 + 1, i * 2 + 2));
        }
        let active_sql = format!(
            "SELECT vctx, vtoken, session_name FROM active_sessions WHERE (vctx, vtoken) IN ({})",
            active_placeholders.join(", ")
        );

        let mut active_q = sqlx::query(&active_sql);
        for (vctx, vtoken) in pairs {
            active_q = active_q.bind(vctx).bind(vtoken);
        }
        let active_rows = active_q.fetch_all(&self.pool).await?;

        // Build map: (vctx, vtoken) → session_name
        let mut session_map: std::collections::HashMap<(String, String), String> =
            std::collections::HashMap::new();
        for row in &active_rows {
            let vctx: String = row.get("vctx");
            let vtoken: String = row.get("vtoken");
            let name: String = row.get("session_name");
            session_map.insert((vctx, vtoken), name);
        }

        // For each pair, use resolved or default session name, then batch-fetch backend IDs.
        let resolved: Vec<(String, String, String)> = pairs
            .iter()
            .map(|(vctx, vtoken)| {
                let name = session_map
                    .get(&(vctx.clone(), vtoken.clone()))
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "default".to_string());
                (vctx.clone(), vtoken.clone(), name)
            })
            .collect();

        let mut session_placeholders = Vec::with_capacity(resolved.len());
        for i in 0..resolved.len() {
            session_placeholders.push(format!(
                "(${}, ${}, ${})",
                i * 3 + 1,
                i * 3 + 2,
                i * 3 + 3
            ));
        }
        let session_sql = format!(
            "SELECT vctx, vtoken, backend_session_id FROM backend_sessions_v2 \
             WHERE (vctx, vtoken, session_name) IN ({})",
            session_placeholders.join(", ")
        );

        let mut session_q = sqlx::query(&session_sql);
        for (vctx, vtoken, name) in &resolved {
            session_q = session_q.bind(vctx).bind(vtoken).bind(name);
        }
        let session_rows = session_q.fetch_all(&self.pool).await?;

        let mut sid_map: std::collections::HashMap<(String, String), String> =
            std::collections::HashMap::new();
        for row in &session_rows {
            let vctx: String = row.get("vctx");
            let vtoken: String = row.get("vtoken");
            let sid: String = row.get("backend_session_id");
            sid_map.insert((vctx, vtoken), sid);
        }

        let mut result = std::collections::HashMap::new();
        for (vctx, vtoken, session_name) in resolved {
            let sid = sid_map
                .get(&(vctx.clone(), vtoken.clone()))
                .filter(|s| !s.trim().is_empty())
                .cloned();
            result.insert((vctx, vtoken), (session_name, sid));
        }
        Ok(result)
    }

    /// Get the active session name for a (vctx, vtoken) pair (defaults to "default").
    pub async fn get_active_session_name(&self, vctx: &str, vtoken: &str) -> Result<String> {
        let row = sqlx::query(
            "SELECT session_name FROM active_sessions WHERE vctx = $1 AND vtoken = $2",
        )
        .bind(vctx)
        .bind(vtoken)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|r| r.get::<String, _>("session_name"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_string()))
    }

    /// Set the active session name for a (vctx, vtoken) pair (upsert).
    pub async fn set_active_session_name(
        &self,
        vctx: &str,
        vtoken: &str,
        session_name: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO active_sessions (vctx, vtoken, session_name)
            VALUES ($1, $2, $3)
            ON CONFLICT (vctx, vtoken) DO UPDATE SET
                session_name = excluded.session_name,
                updated_at   = CURRENT_TIMESTAMP
            "#,
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ─── Context token map ────────────────────────────────────────────────────

    pub async fn map_context_token(&self, real_ctx: &str, peer_user_id: &str) -> Result<String> {
        // Race-free upsert: attempt to insert a fresh vctx for this real_ctx.
        // If another task already inserted the same real_ctx concurrently, the unique
        // index on real_ctx causes a conflict and we fall through to the SELECT below.
        let candidate = format!("vctx_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO context_token_map (vctx, real_ctx, peer_user_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (real_ctx) DO NOTHING",
        )
        .bind(&candidate)
        .bind(real_ctx)
        .bind(peer_user_id)
        .execute(&self.pool)
        .await?;

        // Whether we inserted or hit the conflict, the winner row is now in the table.
        let row = sqlx::query("SELECT vctx FROM context_token_map WHERE real_ctx = $1")
            .bind(real_ctx)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("vctx"))
    }

    /// Persist a known vctx→real_ctx mapping (upsert: refreshes real_ctx when vctx is reused).
    pub async fn persist_context_token(
        &self,
        vctx: &str,
        real_ctx: &str,
        peer_user_id: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO context_token_map (vctx, real_ctx, peer_user_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (vctx) DO UPDATE SET
                real_ctx = excluded.real_ctx,
                peer_user_id = excluded.peer_user_id
            "#,
        )
        .bind(vctx)
        .bind(real_ctx)
        .bind(peer_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist multiple vctx→real_ctx mappings in a single transaction.
    /// Used by the Broadcast dispatch path to avoid N separate round-trips.
    pub async fn persist_context_tokens_batch(
        &self,
        entries: &[(String, String, String)], // (vctx, real_ctx, peer_user_id)
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (vctx, real_ctx, peer_user_id) in entries {
            sqlx::query(
                r#"
                INSERT INTO context_token_map (vctx, real_ctx, peer_user_id)
                VALUES ($1, $2, $3)
                ON CONFLICT (vctx) DO UPDATE SET
                    real_ctx = excluded.real_ctx,
                    peer_user_id = excluded.peer_user_id
                "#,
            )
            .bind(vctx)
            .bind(real_ctx)
            .bind(peer_user_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Find an existing virtual context token for a WeChat peer, preferring one that
    /// already has a persisted backend session (for hub restart / cold cache warm-up).
    pub async fn find_vctx_for_peer(&self, peer_user_id: &str) -> Result<Option<String>> {
        if peer_user_id.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(
            r#"
            SELECT c.vctx FROM context_token_map c
            LEFT JOIN backend_sessions_v2 b
              ON b.vctx = c.vctx AND b.session_name = 'default'
            WHERE c.peer_user_id = $1
              AND b.backend_session_id IS NOT NULL
              AND b.backend_session_id != ''
            LIMIT 1
            "#,
        )
        .bind(peer_user_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = row {
            return Ok(Some(row.get("vctx")));
        }
        let row = sqlx::query("SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1")
            .bind(peer_user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("vctx")))
    }

    pub async fn resolve_context_token(&self, vctx: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT real_ctx FROM context_token_map WHERE vctx = $1")
            .bind(vctx)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("real_ctx")))
    }

    /// Resolve a virtual context token to `(real_ctx, peer_user_id)`.
    pub async fn resolve_context_token_full(&self, vctx: &str) -> Result<Option<(String, String)>> {
        let row = sqlx::query(
            "SELECT real_ctx, COALESCE(peer_user_id, '') AS peer_user_id \
             FROM context_token_map WHERE vctx = $1",
        )
        .bind(vctx)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get("real_ctx"), r.get("peer_user_id"))))
    }

    /// Load the most recent context_token mappings for in-memory cache warm-up.
    /// Returns up to `limit` entries ordered by rowid DESC (newest first).
    pub async fn list_recent_context_tokens(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            "SELECT vctx, real_ctx, COALESCE(peer_user_id, '') AS peer_user_id \
             FROM context_token_map ORDER BY rowid DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("vctx"),
                    r.get::<String, _>("real_ctx"),
                    r.get::<String, _>("peer_user_id"),
                )
            })
            .collect())
    }

    // ─── Bot credentials ──────────────────────────────────────────────────────

    pub async fn save_credentials(&self, token: &str, base_url: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO bot_credentials (id, token, base_url)
            VALUES (1, $1, $2)
            ON CONFLICT (id) DO UPDATE
              SET token = EXCLUDED.token,
                  base_url = EXCLUDED.base_url,
                  updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(token)
        .bind(base_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_credentials(&self) -> Result<Option<(String, String)>> {
        let row = sqlx::query("SELECT token, base_url FROM bot_credentials WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.get("token"), r.get("base_url"))))
    }
}

#[derive(Debug, Clone)]
pub struct ClientRow {
    pub vtoken: String,
    pub name: String,
    pub label: Option<String>,
    pub last_seen: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BackendSessionRow {
    pub session_name: String,
    pub backend_session_id: String,
}
