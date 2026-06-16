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
            let url_owned = url.to_string();
            tokio::task::spawn_blocking(move || Self::ensure_sqlite_file(&url_owned))
                .await
                .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))??;
        }

        // For SQLite :memory: databases each new physical connection gets its own
        // fresh (empty) database. To ensure DDL and DML share the same in-memory
        // instance we pin the pool to a single connection.
        //
        // For file-type SQLite, the same single-connection pin is required to
        // avoid SQLITE_BUSY (5) errors: SQLite's file-level write lock means a
        // long write transaction (e.g. `persist_context_tokens_batch`) and a
        // concurrent read (e.g. `get_active_session_name`) executed on two
        // different physical connections race on the same lock. The
        // default `busy_timeout` of 5s for sqlite connections is kept so
        // the rare case of contention from the migration runner's `acquire`
        // during shutdown still has a chance to drain.
        let pool = if url.starts_with("sqlite:") {
            sqlx::pool::PoolOptions::<sqlx::Any>::new()
                .max_connections(1)
                .connect(url)
                .await?
        } else {
            AnyPool::connect(url).await?
        };
        let store = Self { pool };
        store.run_migrations().await?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn pool(&self) -> &sqlx::AnyPool {
        &self.pool
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

    async fn run_migrations(&self) -> Result<()> {
        // AnyPool does not support DDL (CREATE TABLE / ALTER TABLE) or sqlx::migrate!.
        // For each supported driver we use a typed pool just for the migration run,
        // then close it — the AnyPool used for normal queries shares the same
        // on-disk file (or in-memory instance for SQLite, which is connection-scoped
        // and handled separately).
        //
        // The migration SQL files under migrations/ are the canonical schema record.
        // The Rust code here must stay in sync with those files.

        // ── v1: initial schema ────────────────────────────────────────────────
        self.ddl(
            "CREATE TABLE IF NOT EXISTS clients (
                vtoken      TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                label       TEXT,
                created_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                last_seen   TEXT
            )",
        )
        .await?;

        self.ddl(
            "CREATE TABLE IF NOT EXISTS routing_state (
                from_user     TEXT PRIMARY KEY,
                active_vtoken TEXT NOT NULL,
                updated_at    TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await?;

        self.ddl(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                vctx         TEXT PRIMARY KEY,
                real_ctx     TEXT NOT NULL,
                peer_user_id TEXT NOT NULL DEFAULT '',
                expires_at   TEXT
            )",
        )
        .await?;

        self.ddl(
            "CREATE TABLE IF NOT EXISTS bot_credentials (
                id         INTEGER PRIMARY KEY,
                token      TEXT NOT NULL,
                base_url   TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await?;

        // ── v2: backend session tables ────────────────────────────────────────
        self.ddl(
            "CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                vctx               TEXT NOT NULL,
                vtoken             TEXT NOT NULL,
                session_name       TEXT NOT NULL,
                backend_session_id TEXT NOT NULL DEFAULT '',
                created_at         TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                PRIMARY KEY (vctx, vtoken, session_name)
            )",
        )
        .await?;

        self.ddl(
            "CREATE TABLE IF NOT EXISTS active_sessions (
                vctx         TEXT NOT NULL,
                vtoken       TEXT NOT NULL,
                session_name TEXT NOT NULL DEFAULT 'default',
                updated_at   TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                PRIMARY KEY (vctx, vtoken)
            )",
        )
        .await?;

        // ── v3: real_ctx unique index (race-free upsert) ──────────────────────
        if let Err(e) = self
            .ddl(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_context_token_map_real_ctx \
                 ON context_token_map (real_ctx)",
            )
            .await
        {
            tracing::warn!(error = %e, "v3 migration: CREATE UNIQUE INDEX failed (may already exist)");
        }

        // ── v4: created_at column + index for portable ORDER BY ───────────────
        if let Err(e) = self
            .ddl(
                "ALTER TABLE context_token_map \
                 ADD COLUMN created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)",
            )
            .await
        {
            tracing::debug!(error = %e, "v4 migration: ALTER TABLE skipped (column likely already exists)");
        }

        if let Err(e) = self
            .ddl(
                "CREATE INDEX IF NOT EXISTS idx_context_token_map_created_at \
                 ON context_token_map (created_at DESC)",
            )
            .await
        {
            tracing::warn!(error = %e, "v4 migration: CREATE INDEX failed (may already exist)");
        }

        // ── v5: chat message history ──────────────────────────────────────────
        self.ddl(
            "CREATE TABLE IF NOT EXISTS messages (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                vctx         TEXT NOT NULL,
                vtoken       TEXT,
                session_name TEXT NOT NULL DEFAULT 'default',
                peer_user_id TEXT NOT NULL DEFAULT '',
                role         TEXT NOT NULL,
                content      TEXT NOT NULL,
                created_at   TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await?;

        if let Err(e) = self
            .ddl(
                "CREATE INDEX IF NOT EXISTS idx_messages_vctx_created \
                 ON messages (vctx, created_at DESC)",
            )
            .await
        {
            tracing::warn!(error = %e, "v5 migration: CREATE INDEX failed (may already exist)");
        }

        if let Err(e) = self
            .ddl(
                "CREATE INDEX IF NOT EXISTS idx_messages_peer_role_created \
                 ON messages (peer_user_id, role, created_at DESC)",
            )
            .await
        {
            tracing::warn!(error = %e, "v5 migration: CREATE INDEX (peer) failed (may already exist)");
        }

        Ok(())
    }

    /// Execute a single DDL statement through a pool connection.
    ///
    /// `AnyPool::execute` silently ignores DDL on the pool level. Using an
    /// explicit `PoolConnection` and calling `execute` on the dereffed connection
    /// works correctly, including for SQLite in-memory databases where all
    /// operations must go through the same physical connection.
    async fn ddl(&self, sql: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| anyhow::anyhow!("DDL failed: {sql}\n  Error: {e}"))?;
        Ok(())
    }

    // ─── Clients ─────────────────────────────────────────────────────────────

    pub async fn upsert_client(&self, vtoken: &str, name: &str, label: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Update routing_state for any routes pointing to this client's old vtoken
        // before inserting/updating the client's vtoken.
        sqlx::query(
            r#"
            UPDATE routing_state
            SET active_vtoken = $1
            WHERE active_vtoken = (SELECT vtoken FROM clients WHERE name = $2)
            "#,
        )
        .bind(vtoken)
        .bind(name)
        .execute(&mut *tx)
        .await?;

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
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
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
    ///
    /// Uses `QueryBuilder` so placeholder style (`$N` vs `?`) is chosen automatically
    /// by the runtime driver — compatible with SQLite, PostgreSQL, and MySQL.
    pub async fn get_hub_ext_batch(
        &self,
        pairs: &[(String, String)], // (vctx, vtoken)
    ) -> Result<std::collections::HashMap<(String, String), (String, Option<String>)>> {
        if pairs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Query 1: resolve active session names for each (vctx, vtoken) pair.
        let mut qb = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT vctx, vtoken, session_name FROM active_sessions WHERE ",
        );
        for (i, (vctx, vtoken)) in pairs.iter().enumerate() {
            if i > 0 {
                qb.push(" OR ");
            }
            qb.push("(vctx = ");
            qb.push_bind(vctx.as_str());
            qb.push(" AND vtoken = ");
            qb.push_bind(vtoken.as_str());
            qb.push(")");
        }
        let active_rows = qb.build().fetch_all(&self.pool).await?;

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

        // Query 2: fetch backend session IDs for the resolved (vctx, vtoken, session_name) triples.
        let mut qb2 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT vctx, vtoken, backend_session_id FROM backend_sessions_v2 WHERE ",
        );
        for (i, (vctx, vtoken, name)) in resolved.iter().enumerate() {
            if i > 0 {
                qb2.push(" OR ");
            }
            qb2.push("(vctx = ");
            qb2.push_bind(vctx.as_str());
            qb2.push(" AND vtoken = ");
            qb2.push_bind(vtoken.as_str());
            qb2.push(" AND session_name = ");
            qb2.push_bind(name.as_str());
            qb2.push(")");
        }
        let session_rows = qb2.build().fetch_all(&self.pool).await?;

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
        let row =
            sqlx::query("SELECT session_name FROM active_sessions WHERE vctx = $1 AND vtoken = $2")
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
    ///
    /// All entries are written in one transaction so a partial DB failure does not
    /// leave some rows committed while others are dropped — that would generate
    /// duplicate vctx tokens for the same conversation on the next broadcast.
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
    /// Returns up to `limit` entries ordered by created_at DESC (newest first).
    pub async fn list_recent_context_tokens(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            "SELECT vctx, real_ctx, COALESCE(peer_user_id, '') AS peer_user_id \
             FROM context_token_map ORDER BY created_at DESC LIMIT $1",
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

// ─── Message history ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub vctx: String,
    pub vtoken: Option<String>,
    pub session_name: String,
    pub peer_user_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

impl Store {
    pub async fn save_message(
        &self,
        vctx: &str,
        vtoken: Option<&str>,
        session_name: &str,
        peer_user_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO messages (vctx, vtoken, session_name, peer_user_id, role, content) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .bind(peer_user_id)
        .bind(role)
        .bind(content)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find the most recent assistant message in a conversation whose content starts with
    /// `content_prefix`. Used as DB-backed fallback for quote-reply routing when the
    /// in-memory QuoteRouteIndex is cold (e.g. after a Hub restart).
    pub async fn find_assistant_message_by_content(
        &self,
        peer_user_id: &str,
        content_prefix: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        // LIKE pattern: escape '%' and '_' in the prefix to avoid wildcard interpretation.
        let escaped = content_prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let row = sqlx::query(
            "SELECT vtoken, session_name FROM messages \
             WHERE peer_user_id = $1 AND role = 'assistant' AND content LIKE $2 ESCAPE '\\' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(peer_user_id)
        .bind(&pattern)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let vtoken: Option<String> = r.get("vtoken");
            let session_name: String = r.get("session_name");
            (vtoken.unwrap_or_default(), Some(session_name))
        }))
    }

    pub async fn list_messages(
        &self,
        vctx: &str,
        limit: i64,
    ) -> Result<Vec<MessageRow>> {
        let rows = sqlx::query(
            "SELECT id, vctx, vtoken, session_name, peer_user_id, role, content, created_at \
             FROM messages WHERE vctx = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(vctx)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| MessageRow {
                id: r.get("id"),
                vctx: r.get("vctx"),
                vtoken: r.get("vtoken"),
                session_name: r.get("session_name"),
                peer_user_id: r.get("peer_user_id"),
                role: r.get("role"),
                content: r.get("content"),
                created_at: r.get("created_at"),
            })
            .collect())
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    #[tokio::test]
    async fn migration_runs_on_in_memory_sqlite() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // If migration ran, these should succeed
        let r = store.list_clients().await;
        assert!(r.is_ok(), "list_clients failed: {:?}", r.err());
        let r = store.list_recent_context_tokens(5).await;
        assert!(
            r.is_ok(),
            "list_recent_context_tokens failed: {:?}",
            r.err()
        );
    }

    /// Regression test for DB-01: file-type SQLite must pin the pool to a
    /// single connection so that concurrent write transactions and reads
    /// from different physical connections cannot race on the SQLite file
    /// lock and return `SQLITE_BUSY` (5).
    ///
    /// Before the fix, `AnyPool::connect(url)` for `sqlite:/path/to.db`
    /// defaulted to 10 connections. With multiple tasks issuing write
    /// transactions (`persist_context_tokens_batch`,
    /// `set_active_session_name`) and reads (`get_active_session_name`)
    /// concurrently, two physical connections would race on the
    /// file-level EXCLUSIVE write lock; once a writer's lock-hold time
    /// exceeded the default `busy_timeout` (5s), a competing transaction
    /// would surface `SQLITE_BUSY`. The fix collapses the pool to
    /// `max_connections(1)` for any `sqlite:` URL, which serializes
    /// transactions on a single connection (no second connection means
    /// no second contender for the file lock).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn file_sqlite_serializes_concurrent_read_and_write_without_busy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("concurrent.db");
        let url = format!("sqlite:{}", db_path.display());
        let store = std::sync::Arc::new(Store::connect(&url).await.expect("connect"));

        // The fix is structural: the pool must be sized to a single
        // connection for any sqlite URL. Verify the invariant first
        // (fast, deterministic, pinpoints regressions), then run a
        // multi-task mixed read/write workload that would surface
        // SQLITE_BUSY on a multi-connection pool with a non-default
        // (small) busy_timeout. The structural assertion is the
        // canonical regression guard.
        assert_eq!(
            store.pool.options().get_max_connections(),
            1,
            "SQLite pool must be pinned to max_connections(1) to avoid SQLITE_BUSY"
        );

        // Seed one row so the read path has a target.
        store
            .persist_context_token("vctx-seed", "real-ctx-seed", "peer-seed")
            .await
            .expect("seed");
        store
            .set_active_session_name("vctx-seed", "vtoken-seed", "default")
            .await
            .expect("seed active session");

        let mut handles = Vec::new();

        // Batch-write task: hammer persist_context_tokens_batch with bulk
        // entries to lengthen each transaction and increase the chance of a
        // write/write race on the file lock. Each task runs 20 iterations of
        // a 200-entry batch.
        for w in 0..8 {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                for i in 0..20 {
                    let entries: Vec<(String, String, String)> = (0..200)
                        .map(|j| {
                            (
                                format!("vctx-w{w}-i{i}-j{j}"),
                                format!("real-ctx-w{w}-i{i}-j{j}"),
                                format!("peer-w{w}-i{i}-j{j}"),
                            )
                        })
                        .collect();
                    store
                        .persist_context_tokens_batch(&entries)
                        .await
                        .expect("batch write must not fail");
                }
            }));
        }

        // Single-row write task: hammer set_active_session_name (a
        // write transaction) on a different row each time so we are
        // exercising the same physical-connection file-lock path.
        for w in 0..4 {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                for i in 0..200 {
                    let vctx = format!("vctx-active-w{w}-i{i}");
                    let vtoken = format!("vtoken-active-w{w}-i{i}");
                    store
                        .set_active_session_name(&vctx, &vtoken, "default")
                        .await
                        .expect("set_active_session_name must not fail");
                }
            }));
        }

        // Reader task: hammer get_active_session_name.
        for r in 0..4 {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                for i in 0..200 {
                    let vtoken = format!("ignored-vtoken-r{r}-i{i}");
                    let name = store
                        .get_active_session_name("vctx-seed", &vtoken)
                        .await
                        .expect("read must not fail");
                    assert_eq!(name, "default");
                }
            }));
        }

        for h in handles {
            h.await.expect("task join");
        }
    }

    #[tokio::test]
    async fn test_sync_02_upsert_client_updates_routing_state() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");

        // Register client "bridge-a" with "vtoken-1"
        store
            .upsert_client("vtoken-1", "bridge-a", None)
            .await
            .unwrap();

        // Set route for user "alice" to "vtoken-1"
        store.set_route("alice", "vtoken-1").await.unwrap();

        // Verify route is set
        let route = store.get_route("alice").await.unwrap();
        assert_eq!(route, Some("vtoken-1".to_string()));

        // Re-register client "bridge-a" with "vtoken-2"
        store
            .upsert_client("vtoken-2", "bridge-a", None)
            .await
            .unwrap();

        // Verify route is updated to "vtoken-2"
        let route = store.get_route("alice").await.unwrap();
        assert_eq!(route, Some("vtoken-2".to_string()));
    }

    #[tokio::test]
    async fn test_db_03_get_hub_ext_batch_query() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");

        // Insert some session data
        store
            .set_active_session_name("vctx-1", "vtoken-1", "session-1")
            .await
            .unwrap();
        store
            .set_active_session_name("vctx-2", "vtoken-2", "session-2")
            .await
            .unwrap();

        store
            .set_backend_session("vctx-1", "vtoken-1", "session-1", "sid-1")
            .await
            .unwrap();
        store
            .set_backend_session("vctx-2", "vtoken-2", "session-2", "sid-2")
            .await
            .unwrap();

        let pairs = vec![
            ("vctx-1".to_string(), "vtoken-1".to_string()),
            ("vctx-2".to_string(), "vtoken-2".to_string()),
            ("vctx-3".to_string(), "vtoken-3".to_string()), // nonexistent
        ];

        let result = store.get_hub_ext_batch(&pairs).await.unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(
            result.get(&("vctx-1".to_string(), "vtoken-1".to_string())),
            Some(&("session-1".to_string(), Some("sid-1".to_string())))
        );
        assert_eq!(
            result.get(&("vctx-2".to_string(), "vtoken-2".to_string())),
            Some(&("session-2".to_string(), Some("sid-2".to_string())))
        );
        assert_eq!(
            result.get(&("vctx-3".to_string(), "vtoken-3".to_string())),
            Some(&("default".to_string(), None))
        );
    }

    #[tokio::test]
    async fn test_db_02_persist_context_tokens_batch_large() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");

        // Prepare 55 entries
        let mut entries = Vec::new();
        for i in 0..55 {
            entries.push((
                format!("vctx-{}", i),
                format!("real-{}", i),
                format!("peer-{}", i),
            ));
        }

        store.persist_context_tokens_batch(&entries).await.unwrap();

        // Check if entries are saved
        let recent = store.list_recent_context_tokens(100).await.unwrap();
        assert_eq!(recent.len(), 55);
    }

    #[tokio::test]
    async fn test_sync_02_upsert_client_concurrent_adversarial() {
        // Create a temporary database in target/ directory of the workspace
        let temp_dir = tempfile::Builder::new()
            .prefix("test_concurrent_db")
            .tempdir_in("target")
            .unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db_url = format!("sqlite:{}", db_path.to_str().unwrap());

        let store = Store::connect(&db_url).await.expect("connect");

        // Initial setup: register client "bridge-concurrent" with "vtoken-initial"
        store
            .upsert_client("vtoken-initial", "bridge-concurrent", None)
            .await
            .unwrap();

        // Set route for user "alice" to "vtoken-initial"
        store.set_route("alice", "vtoken-initial").await.unwrap();

        // Now run multiple concurrent upserts of client "bridge-concurrent"
        let num_concurrency = 20;
        let mut handles = vec![];

        let store = std::sync::Arc::new(store);

        for i in 0..num_concurrency {
            let store_clone = store.clone();
            let vtoken = format!("vtoken-{}", i);
            let handle = tokio::spawn(async move {
                store_clone
                    .upsert_client(&vtoken, "bridge-concurrent", None)
                    .await
            });
            handles.push(handle);
        }

        // Wait for all tasks to complete
        for h in handles {
            h.await.unwrap().unwrap();
        }

        // Retrieve the final vtoken in the clients table
        let clients = store.list_clients().await.unwrap();
        let final_client_vtoken = clients
            .iter()
            .find(|c| c.name == "bridge-concurrent")
            .map(|c| c.vtoken.clone())
            .unwrap();

        // Retrieve the route for "alice"
        let final_route = store.get_route("alice").await.unwrap().unwrap();

        // Under race conditions in the old implementation, final_route would be stale
        // while final_client_vtoken would be the last committed vtoken.
        // We assert that they must be identical.
        assert_eq!(final_route, final_client_vtoken);
    }
}
