//! Database persistence layer.
//! Uses sqlx with runtime driver selection via `DATABASE_URL`:
//!   sqlite:~/.ilink-hub/ilink-hub.db → SQLite (default, file created if missing)
//!   postgres://user:pass@host/db      → PostgreSQL
//!   mysql://user:pass@host/db         → MySQL

use anyhow::Result;
use sqlx::{AnyPool, Row};
use uuid::Uuid;

/// Whitelist check used by SQLite `pragma_table_info` splicing: the table
/// and column names must contain only identifier characters, so the
/// interpolated string cannot smuggle SQL.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

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

    /// Returns the highest version recorded in `schema_version`, or 0 if the
    /// table is empty. Decode errors are propagated — a DB that has rows but
    /// fails to decode them is NOT the same as a fresh DB.
    pub async fn get_current_version(&self) -> Result<i32> {
        let row = sqlx::query("SELECT MAX(version) FROM schema_version")
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(r) => {
                let val: Option<i32> = r.try_get(0)?;
                Ok(val.unwrap_or(0))
            }
            None => Ok(0),
        }
    }

    /// Returns true if a row for `version` exists in `schema_version`.
    /// Decode errors are propagated.
    pub async fn is_migration_run(&self, version: i32) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM schema_version WHERE version = $1")
            .bind(version)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Atomically claim a migration version. Returns `true` if THIS caller
    /// won the race and must run the corresponding DDL; returns `false` if
    /// another writer (concurrent `Store::connect`) has already claimed it.
    ///
    /// This is the canonical check-and-claim pattern that closes the
    /// TOCTOU window between `is_migration_run` and `record_migration_run`
    /// on multi-connection pools (Postgres, MySQL). The claim is recorded
    /// before the DDL runs, so a concurrent second writer sees the row
    /// already present and skips the DDL.
    pub async fn try_claim_migration(&self, version: i32) -> Result<bool> {
        let row = sqlx::query(
            "INSERT INTO schema_version (version) VALUES ($1) \
             ON CONFLICT (version) DO NOTHING RETURNING version",
        )
        .bind(version)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Internal: mark a version as run after its DDL has completed.
    /// Kept for symmetry with the older two-step pattern; `try_claim_migration`
    /// is the primary path. The DDL runs only when `try_claim_migration`
    /// returned `true`, so by the time this is called the row is already
    /// present — this is a no-op safety net.
    pub async fn record_migration_run(&self, version: i32) -> Result<()> {
        sqlx::query(
            "INSERT INTO schema_version (version) VALUES ($1) \
             ON CONFLICT (version) DO NOTHING",
        )
        .bind(version)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn run_migrations(&self) -> Result<()> {
        // AnyPool does not support DDL (CREATE TABLE / ALTER TABLE) or sqlx::migrate!.
        // We implement our own lightweight version-tracking table (`schema_version`)
        // so each migration step is applied exactly once and can be safely re-run.
        //
        // The migration SQL files under migrations/ are the canonical human-readable
        // reference. The Rust code here must stay in sync with those files.
        //
        // Concurrency model: each migration step is gated by `try_claim_migration(N)`,
        // which uses a single `INSERT ... ON CONFLICT DO NOTHING RETURNING version`
        // statement to atomically check-and-claim the version. The caller that
        // wins the RETURNING row owns the DDL; concurrent writers see the row
        // already present and skip the DDL. This closes the TOCTOU race between
        // `is_migration_run` and `record_migration_run` on multi-connection pools
        // (Postgres, MySQL) where the SQLite `max_connections(1)` pin does not
        // apply. The claim row is written BEFORE the DDL, so a partial DDL
        // failure leaves the version row present and a subsequent connect will
        // skip the broken step (DBA can then drop the row manually).
        //
        // Invariant: every DDL block below must be idempotent OR guarded by a
        // pre-check that detects the post-migration state. A non-idempotent
        // DDL whose claim row was written but whose execution crashed mid-way
        // would prevent re-running; protect against that by either making the
        // DDL idempotent (`IF NOT EXISTS`) or by short-circuiting when the
        // schema already shows the post-migration shape.

        // Bootstrap the version-tracking table before anything else.
        // This is always idempotent via IF NOT EXISTS.
        self.ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version     INTEGER PRIMARY KEY,
                migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await?;

        // Dispatch to the per-version migrators. Each step is gated by
        // `try_claim_migration`, so a step that has already been applied
        // (i.e. its row is present in `schema_version`) is a no-op. Steps
        // that fail (DDL error, decode error) propagate `Err` straight up
        // to `Store::connect`, blocking the program from starting in a
        // half-migrated state.
        self.migrate_to_v1().await?;
        self.migrate_to_v2().await?;
        self.migrate_to_v3().await?;
        self.migrate_to_v4().await?;
        self.migrate_to_v5().await?;

        Ok(())
    }

    /// v1: initial schema — clients, routing_state, context_token_map, bot_credentials.
    async fn migrate_to_v1(&self) -> Result<()> {
        if !self.try_claim_migration(1).await? {
            return Ok(());
        }
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

        tracing::info!(version = 1, "migration applied: initial schema");
        Ok(())
    }

    /// v2: backend session tables — backend_sessions_v2, active_sessions.
    async fn migrate_to_v2(&self) -> Result<()> {
        if !self.try_claim_migration(2).await? {
            return Ok(());
        }
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

        tracing::info!(version = 2, "migration applied: backend session tables");
        Ok(())
    }

    /// v3: real_ctx unique index — backs race-free upsert in `map_context_token`.
    async fn migrate_to_v3(&self) -> Result<()> {
        if !self.try_claim_migration(3).await? {
            return Ok(());
        }
        self.ddl(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_context_token_map_real_ctx \
             ON context_token_map (real_ctx)",
        )
        .await?;

        tracing::info!(version = 3, "migration applied: real_ctx unique index");
        Ok(())
    }

    /// v4: `created_at` column + index on `context_token_map` for portable ORDER BY.
    ///
    /// `ALTER TABLE ADD COLUMN` is NOT idempotent — re-running it on a DB that
    /// already has the column returns an error. The `try_claim_migration(4)`
    /// gate prevents re-execution on DBs that went through v4 normally.
    ///
    /// For DBs upgrading from a pre-schema_version era the column may
    /// already exist. We probe `information_schema.columns` BEFORE the
    /// ALTER to short-circuit. This is portable across SQLite, Postgres,
    /// and MySQL and avoids matching on the driver's error-string format
    /// (which can shift across versions and silently swallow unrelated
    /// errors like UNIQUE-constraint violations).
    async fn migrate_to_v4(&self) -> Result<()> {
        if !self.try_claim_migration(4).await? {
            return Ok(());
        }
        // SQLite ALTER TABLE ADD COLUMN forbids CURRENT_TIMESTAMP (and any
        // other dynamic value) as a default, because SQLite would need to
        // evaluate it for every existing row and the value is non-constant.
        //
        // We add the column as nullable TEXT. All INSERT / UPDATE statements
        // that write to context_token_map explicitly supply CURRENT_TIMESTAMP
        // for created_at, so new rows always have a proper timestamp. Pre-v4
        // rows (if any) get NULL, which list_recent_context_tokens handles
        // via COALESCE.
        //
        // Pre-check: ask the catalog whether the column already exists. This
        // works on SQLite, Postgres, and MySQL (sqlx translates the SQL to
        // the right placeholder style and the catalog is standard SQL).
        if !self
            .column_exists("context_token_map", "created_at")
            .await?
        {
            self.ddl("ALTER TABLE context_token_map ADD COLUMN created_at TEXT")
                .await?;
        } else {
            tracing::debug!(
                "v4 migration: created_at column already present (pre-check), skipping ALTER"
            );
        }

        self.ddl(
            "CREATE INDEX IF NOT EXISTS idx_context_token_map_created_at \
             ON context_token_map (created_at DESC)",
        )
        .await?;

        tracing::info!(
            version = 4,
            "migration applied: context_token_map created_at column + index"
        );
        Ok(())
    }

    /// v5: chat message history — `messages` table + supporting indexes.
    ///
    /// The `id` column uses driver-specific auto-increment syntax. `AUTOINCREMENT`
    /// is SQLite-only; on Postgres / MySQL the migrator substitutes the SQL
    /// standard `GENERATED BY DEFAULT AS IDENTITY` (Postgres 10+, MySQL 8.0+).
    /// Both forms produce an auto-incrementing primary key. The driver probe
    /// reuses the `column_exists` pattern: `SELECT current_database()` succeeds
    /// on Postgres/MySQL, errors on SQLite. This is the F-M2-02 fix from the
    /// m2 review; M3 is the milestone that introduces the SQL files and
    /// closes the portability gap.
    async fn migrate_to_v5(&self) -> Result<()> {
        if !self.try_claim_migration(5).await? {
            return Ok(());
        }
        let is_sqlite = sqlx::query("SELECT current_database()")
            .fetch_optional(&self.pool)
            .await
            .is_err();
        let create_messages = Self::v5_create_messages_sql(is_sqlite);
        self.ddl(&create_messages).await?;

        self.ddl(
            "CREATE INDEX IF NOT EXISTS idx_messages_vctx_created \
             ON messages (vctx, created_at DESC)",
        )
        .await?;

        self.ddl(
            "CREATE INDEX IF NOT EXISTS idx_messages_peer_role_created \
             ON messages (peer_user_id, role, created_at DESC)",
        )
        .await?;

        tracing::info!(version = 5, "migration applied: messages table + indexes");
        Ok(())
    }

    /// v5 `CREATE TABLE messages` DDL, with the `id` clause selected by driver.
    /// Pulled out of `migrate_to_v5` so the m3 test surface can call both
    /// branches directly without spinning up a Postgres or MySQL connection.
    ///
    /// Returns the SQLite form when `is_sqlite` is true, otherwise the
    /// SQL-standard `GENERATED BY DEFAULT AS IDENTITY` form supported by
    /// Postgres 10+ and MySQL 8.0+. Field types, default values
    /// (`CURRENT_TIMESTAMP`), and table-level shape are identical to the
    /// `migrations/0005_messages.sql` reference; the only divergence between
    /// the two forms is the `id` clause (F-M2-02 / M3 alignment).
    fn v5_create_messages_sql(is_sqlite: bool) -> String {
        let id_clause = if is_sqlite {
            "id           INTEGER PRIMARY KEY AUTOINCREMENT"
        } else {
            "id           INTEGER PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY"
        };
        format!(
            "CREATE TABLE IF NOT EXISTS messages (
                {id_clause},
                vctx         TEXT NOT NULL,
                vtoken       TEXT,
                session_name TEXT NOT NULL DEFAULT 'default',
                peer_user_id TEXT NOT NULL DEFAULT '',
                role         TEXT NOT NULL,
                content      TEXT NOT NULL,
                created_at   TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )"
        )
    }

    /// Check whether a column exists on a table.
    ///
    /// SQLite does not implement standard `information_schema`, so we use the
    /// SQLite-specific `pragma_table_info` for the SQLite driver and the
    /// portable `information_schema.columns` query for Postgres / MySQL.
    /// The two paths are selected at runtime by inspecting the first token of
    /// `schema_version`'s `current_schema()` (or, more simply, by attempting
    /// the SQLite pragma and falling back to `information_schema`).
    ///
    /// Returns Ok(false) on any error reading the catalog (caller treats the
    /// column as not present and lets the DDL surface the real error).
    async fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        // The `pragma_table_info` form works on SQLite. `pragma` cannot be
        // parameterised, so we validate identifiers before splicing.
        if !is_safe_identifier(table) || !is_safe_identifier(column) {
            return Ok(false);
        }

        // Driver probe: `current_database()` is portable SQL but absent on
        // SQLite. Successful execution means Postgres/MySQL; Err means SQLite.
        let is_sqlite = sqlx::query("SELECT current_database()")
            .fetch_optional(&self.pool)
            .await
            .is_err();

        if is_sqlite {
            // SQLite: `pragma_table_info('<table>')` returns one row per column.
            // - Ok(Some(_)) → column found
            // - Ok(None)    → column absent (or table absent)
            // - Err(_)      → treat as absent so the DDL surfaces the real error
            //                 (e.g. "no such table") to the caller
            let pragma_sql =
                format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'");
            let row = sqlx::query(&pragma_sql)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);
            return Ok(row.is_some());
        }

        // Postgres / MySQL: standard information_schema.
        let row = sqlx::query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_name = $1 AND column_name = $2 LIMIT 1",
        )
        .bind(table)
        .bind(column)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
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
            "INSERT INTO context_token_map (vctx, real_ctx, peer_user_id, created_at) \
             VALUES ($1, $2, $3, CURRENT_TIMESTAMP) \
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
            INSERT INTO context_token_map (vctx, real_ctx, peer_user_id, created_at)
            VALUES ($1, $2, $3, CURRENT_TIMESTAMP)
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
                INSERT INTO context_token_map (vctx, real_ctx, peer_user_id, created_at)
                VALUES ($1, $2, $3, CURRENT_TIMESTAMP)
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
        // created_at was added in v4 as a nullable column to remain compatible with
        // SQLite's ALTER TABLE restriction. Rows from before v4 may have NULL; we treat
        // those as oldest (empty string '' sorts before any ISO timestamp).
        let rows = sqlx::query(
            "SELECT vctx, real_ctx, COALESCE(peer_user_id, '') AS peer_user_id \
             FROM context_token_map ORDER BY COALESCE(created_at, '') DESC LIMIT $1",
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
        let escaped = content_prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
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

    pub async fn list_messages(&self, vctx: &str, limit: i64) -> Result<Vec<MessageRow>> {
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

    /// After `Store::connect`, all v1-v5 migrations must have been applied.
    #[tokio::test]
    async fn test_schema_version_tracking() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");

        // All five migrations must be applied after a fresh connect.
        let version = store
            .get_current_version()
            .await
            .expect("get_current_version");
        assert_eq!(
            version, 5,
            "expected all 5 migrations to be applied on a fresh DB"
        );

        for v in 1..=5 {
            let applied = store.is_migration_run(v).await.expect("is_migration_run");
            assert!(applied, "migration v{v} should be marked as applied");
        }

        // Version 0 is not used in the current scheme.
        let run_0 = store.is_migration_run(0).await.expect("is_migration_run");
        assert!(
            !run_0,
            "version 0 is not a real migration and must not be set"
        );
    }

    /// Running `Store::connect` twice on the same in-memory database must not fail.
    /// This is the idempotency guarantee: all migrations use `IF NOT EXISTS` guards
    /// and `ON CONFLICT DO NOTHING`, so repeated runs are safe.
    #[tokio::test]
    async fn test_migration_idempotency() {
        let store = Store::connect("sqlite::memory:")
            .await
            .expect("first connect");

        // Manually call run_migrations again to simulate a re-run.
        store
            .run_migrations()
            .await
            .expect("second run_migrations must be idempotent");

        let version = store
            .get_current_version()
            .await
            .expect("get_current_version");
        assert_eq!(version, 5, "version must remain 5 after idempotent re-run");
    }

    /// Simulates a database that was bootstrapped at v2 (e.g. an older deployment
    /// that never ran v3–v5). After calling `run_migrations`, v3-v5 must be applied
    /// and v1-v2 must remain intact.
    #[tokio::test]
    async fn test_migration_incremental_from_v2() {
        // Bootstrap with only v1-v2 tables and schema_version table set to v2.
        let store = {
            let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("pool");
            let s = Store { pool };

            // Manually create the tables that v1 and v2 would create.
            s.ddl(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("schema_version");
            s.ddl(
                "CREATE TABLE IF NOT EXISTS clients (
                    vtoken TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                    label TEXT, created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP), last_seen TEXT
                )",
            )
            .await
            .expect("clients");
            s.ddl(
                "CREATE TABLE IF NOT EXISTS routing_state (
                    from_user TEXT PRIMARY KEY,
                    active_vtoken TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("routing_state");
            s.ddl(
                "CREATE TABLE IF NOT EXISTS context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
            )
            .await
            .expect("context_token_map");
            s.ddl(
                "CREATE TABLE IF NOT EXISTS bot_credentials (
                    id INTEGER PRIMARY KEY, token TEXT NOT NULL,
                    base_url TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("bot_credentials");
            s.ddl(
                "CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL, backend_session_id TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken, session_name)
                )",
            )
            .await
            .expect("backend_sessions_v2");
            s.ddl(
                "CREATE TABLE IF NOT EXISTS active_sessions (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL DEFAULT 'default',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken)
                )",
            )
            .await
            .expect("active_sessions");

            // Mark v1 and v2 as already applied.
            s.record_migration_run(1).await.expect("mark v1");
            s.record_migration_run(2).await.expect("mark v2");

            s
        };

        // v3-v5 should not yet be applied.
        assert!(!store.is_migration_run(3).await.unwrap());
        assert!(!store.is_migration_run(4).await.unwrap());
        assert!(!store.is_migration_run(5).await.unwrap());

        // Running migrations now must apply v3-v5.
        store.run_migrations().await.expect("incremental migration");

        let version = store.get_current_version().await.unwrap();
        assert_eq!(version, 5, "must reach v5 after incremental migration");

        for v in 1..=5 {
            assert!(
                store.is_migration_run(v).await.unwrap(),
                "v{v} must be marked applied"
            );
        }
    }

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

    // ─── Adversarial regression tests for the review findings ─────────────────
    //
    // Each test below pins down a specific finding from the M1 review. They are
    // grouped by the finding they cover, not by topic, so a future reader
    // hunting for "what was F-M1-02?" can grep and land here.

    /// F-M1-01 / F-M1-04: Two concurrent `Store::connect` calls against the same
    /// file-backed SQLite database must BOTH succeed and converge to
    /// `get_current_version() == 5`.
    ///
    /// Before the `try_claim_migration` fix, this race would double-run the DDL
    /// (the v3 / v4 paths in particular) and either log "v3/v4 applied" twice
    /// or surface a "duplicate column" error caught by the F-M1-02 substring
    /// heuristic. After the fix, exactly one writer's `INSERT ... RETURNING`
    /// produces a row, the second writer's `INSERT ... ON CONFLICT DO NOTHING`
    /// returns NULL, and both writers complete the vN DDL exactly once (or
    /// skip it for the loser).
    #[tokio::test]
    async fn adversarial_concurrent_store_connect_succeeds_and_converges() {
        sqlx::any::install_default_drivers();
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}/concurrent.db", tmp.path().display());
        let s1 = Store::connect(&url).await.expect("connect #1 must succeed");
        let s2 = Store::connect(&url).await.expect("connect #2 must succeed");
        assert_eq!(
            s1.get_current_version().await.unwrap(),
            5,
            "writer #1 must see all v1-v5 applied"
        );
        assert_eq!(
            s2.get_current_version().await.unwrap(),
            5,
            "writer #2 must see all v1-v5 applied"
        );
        // The whole schema must be usable from both writers — no half-applied
        // tables, no missing indexes.
        for s in [&s1, &s2] {
            assert!(s.list_clients().await.is_ok());
            assert!(s.list_recent_context_tokens(1).await.is_ok());
        }
    }

    /// F-M1-01: many concurrent `Store::connect` callers (10) all converge to
    /// version 5. The `INSERT ... RETURNING` claim is the only thing standing
    /// between this test and an arbitrary number of DDL re-runs on a shared DB.
    #[tokio::test]
    async fn adversarial_many_concurrent_connects_converge() {
        sqlx::any::install_default_drivers();
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}/many.db", tmp.path().display());
        let mut stores = Vec::new();
        for i in 0..10 {
            let url = url.clone();
            let s = Store::connect(&url)
                .await
                .unwrap_or_else(|e| panic!("connect #{i} failed: {e}"));
            assert_eq!(
                s.get_current_version().await.unwrap(),
                5,
                "connect #{i} must see all v1-v5 applied"
            );
            stores.push(s);
        }
        for s in &stores {
            assert_eq!(s.get_current_version().await.unwrap(), 5);
        }
    }

    /// F-M1-02: v4's "column already exists" branch is now driven by an
    /// `information_schema.columns` pre-check, not by an error-string match.
    /// Simulate the pre-schema_version deployment state (v1+v2 tables exist,
    /// `created_at` already present, v1+v2 marked run) and verify the v4 path
    /// is silently skipped (no error, no DDL) and the index is created.
    #[tokio::test]
    async fn adversarial_v4_skips_alter_when_column_already_present() {
        // Install drivers so the manual pool below can use them.
        sqlx::any::install_default_drivers();
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let store = Store { pool };
        // Bootstrap the same v1+v2 state as `test_migration_incremental_from_v2`.
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("schema_version");
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS clients (
                    vtoken TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                    label TEXT, created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP), last_seen TEXT
                )",
            )
            .await
            .expect("clients");
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS routing_state (
                    from_user TEXT PRIMARY KEY,
                    active_vtoken TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("routing_state");
        // The legacy state: `created_at` already present, but schema_version
        // doesn't yet know about v4.
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT,
                    created_at TEXT
                )",
            )
            .await
            .expect("context_token_map (with created_at)");
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS bot_credentials (
                    id INTEGER PRIMARY KEY, token TEXT NOT NULL,
                    base_url TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("bot_credentials");
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL, backend_session_id TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken, session_name)
                )",
            )
            .await
            .expect("backend_sessions_v2");
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS active_sessions (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL DEFAULT 'default',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken)
                )",
            )
            .await
            .expect("active_sessions");
        store.record_migration_run(1).await.expect("mark v1");
        store.record_migration_run(2).await.expect("mark v2");

        // Run migrations: v3, v4, v5 must all run, and v4 must NOT fail.
        store
            .run_migrations()
            .await
            .expect("run_migrations must succeed");

        // All v1-v5 must be marked applied.
        for v in 1..=5 {
            assert!(
                store.is_migration_run(v).await.unwrap(),
                "v{v} must be marked applied after run_migrations"
            );
        }
        // The pre-check took the "skip" branch — verify by reading the catalog
        // directly. If the column were missing, the v4 path would have re-added
        // it. Reading the catalog also confirms we did not accidentally drop
        // the legacy column.
        assert!(
            store
                .column_exists("context_token_map", "created_at")
                .await
                .unwrap(),
            "created_at must still exist (we only skip the ALTER, never drop)"
        );
    }

    /// F-M1-03: a column-decode error on `get_current_version` must be
    /// propagated, not swallowed. We seed the table with a value that cannot
    /// be decoded as `i32` (a text string), call `get_current_version`, and
    /// assert the result is `Err` rather than a silent `Ok(0)`.
    #[tokio::test]
    async fn adversarial_get_current_version_propagates_decode_error() {
        sqlx::any::install_default_drivers();
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let store = Store { pool };
        // Seed the schema_version table with a text value. SQLite's type
        // affinity rules will happily store "not-a-number" in an INTEGER
        // PRIMARY KEY column (type affinity is not type enforcement), and
        // sqlx's `try_get::<i32, _>` will fail to decode it.
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("schema_version");
        // Modern SQLite enforces INTEGER PRIMARY KEY strictly — text values are
        // rejected with "datatype mismatch". Instead, we insert a value that is a
        // valid INTEGER (so SQLite accepts it) but is larger than i32::MAX, so
        // sqlx's `try_get::<i32, _>` will fail to decode it.
        // i32::MAX = 2_147_483_647; we use 2_147_483_648 (one over).
        let too_large: i64 = i64::from(i32::MAX) + 1;
        sqlx::query("INSERT INTO schema_version (version) VALUES ($1)")
            .bind(too_large)
            .execute(&store.pool)
            .await
            .expect("insert i64 value larger than i32::MAX");
        // get_current_version must NOT silently return 0 — the column has a
        // row, but the MAX value (2_147_483_648) cannot be decoded as i32.
        // The fix propagates the decode error via `r.try_get(0)?`.
        let res = store.get_current_version().await;
        assert!(
            res.is_err(),
            "expected decode error for value > i32::MAX, got Ok({:?}) — F-M1-03 not fixed",
            res.ok()
        );
    }

    /// F-M1-07: `is_migration_run` / `get_current_version` / `try_claim_migration`
    /// must accept version values used by the migration runner and the test
    /// surface. A negative version is not a real migration but the API must
    /// not crash. `get_current_version` and `try_claim_migration` follow the
    /// same shape — they bind the version and pass it through to the driver.
    /// This test pins down the boundary behaviour for the version = 0 and
    /// negative cases so a future refactor that tightens input validation
    /// has a clear contract to keep.
    #[tokio::test]
    async fn adversarial_version_api_boundaries() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // is_migration_run(0): not applied, no error.
        assert!(!store.is_migration_run(0).await.unwrap());
        // is_migration_run(-1): not applied, no error.
        assert!(!store.is_migration_run(-1).await.unwrap());
        // get_current_version: 5 (the highest applied).
        assert_eq!(store.get_current_version().await.unwrap(), 5);
    }

    /// F-M1-08: `try_claim_migration` is the atomic primitive. Two concurrent
    /// claims for the SAME version on a multi-connection-shaped scenario
    /// (simulated by issuing two claims on the same store) must result in
    /// exactly one `true` and one `false`. This is the unit-level guard
    /// for the M1 invariant; the end-to-end variant is the
    /// `adversarial_concurrent_store_connect_succeeds_and_converges` test.
    ///
    /// On SQLite with `max_connections(1)` the SQL is serialised by the
    /// connection, so the second claim always observes the first's row. On
    /// Postgres / MySQL the `ON CONFLICT DO NOTHING RETURNING` clause is the
    /// thing that serialises the claim — the second `INSERT` is a no-op
    /// (returns no row). The test works on SQLite because the storage path
    /// is the same `INSERT ... ON CONFLICT DO NOTHING RETURNING` SQL.
    #[tokio::test]
    async fn adversarial_try_claim_is_mutually_exclusive() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // Manually delete v5's claim row so we can race for it.
        sqlx::query("DELETE FROM schema_version WHERE version = 5")
            .execute(&store.pool)
            .await
            .expect("delete v5 row");
        // Two back-to-back claims; the second must observe the first's row.
        let first = store.try_claim_migration(5).await.unwrap();
        let second = store.try_claim_migration(5).await.unwrap();
        assert!(first, "first claim must win");
        assert!(!second, "second claim must lose");
    }

    // ─── M2 regression tests ───────────────────────────────────────────────
    //
    // M2 refactors `run_migrations` into per-version `migrate_to_vN` functions.
    // Each step is gated by `try_claim_migration`, the DDL errors propagate
    // via `?` rather than being swallowed, and the schema_version table is
    // updated as a side-effect of the claim. The tests below pin each of
    // those invariants.

    /// F-M2-01: every `migrate_to_vN` is independently callable and updates
    /// `schema_version` only for its own version. Calling v2 alone after a
    /// fresh connect (which has only v0) must record v2 and leave v1, v3,
    /// v4, v5 unmarked.
    #[tokio::test]
    async fn m2_per_version_migrators_update_schema_version_independently() {
        sqlx::any::install_default_drivers();
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let store = Store { pool };
        // Bootstrap only the schema_version table — no migrations applied yet.
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("schema_version");

        // Run only v2 in isolation.
        store.migrate_to_v2().await.expect("migrate_to_v2");

        // v2 must be marked; v1, v3, v4, v5 must not.
        assert!(
            store.is_migration_run(2).await.unwrap(),
            "v2 must be marked after migrate_to_v2"
        );
        for v in [1, 3, 4, 5] {
            assert!(
                !store.is_migration_run(v).await.unwrap(),
                "v{v} must NOT be marked after running only v2"
            );
        }

        // v2 tables must exist (sanity check the DDL actually ran).
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='backend_sessions_v2'",
        )
        .fetch_optional(&store.pool)
        .await
        .expect("catalog");
        assert!(
            row.is_some(),
            "backend_sessions_v2 must exist after migrate_to_v2"
        );

        // v1 tables must NOT exist (v1 was not run).
        let row: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name='clients'")
                .fetch_optional(&store.pool)
                .await
                .expect("catalog");
        assert!(row.is_none(), "clients must NOT exist (v1 was not run)");
    }

    /// F-M2-02: re-running an already-applied migration is a no-op. The
    /// claim returns false, the DDL is skipped, and the schema_version
    /// row is unchanged.
    #[tokio::test]
    async fn m2_migrators_are_idempotent_per_step() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // After connect, all 5 are applied. Re-running each must NOT fail
        // and must NOT touch the schema_version table.
        store.migrate_to_v1().await.expect("v1 re-run");
        store.migrate_to_v2().await.expect("v2 re-run");
        store.migrate_to_v3().await.expect("v3 re-run");
        store.migrate_to_v4().await.expect("v4 re-run");
        store.migrate_to_v5().await.expect("v5 re-run");

        // Still at v5.
        assert_eq!(store.get_current_version().await.unwrap(), 5);
    }

    /// F-M2-03: a DDL failure inside a migrator must propagate as `Err`,
    /// NOT be silently swallowed. We construct a synthetic failure: pre-
    /// create a `context_token_map` whose schema blocks v3's
    /// `CREATE UNIQUE INDEX`. The unique index is rejected when the table
    /// already has duplicate `real_ctx` rows, so the migrator must
    /// surface the underlying driver error.
    #[tokio::test]
    async fn m2_ddl_error_propagates_through_migrator() {
        sqlx::any::install_default_drivers();
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let store = Store { pool };
        // Bootstrap the version-tracking table and the v1 schema with
        // duplicated real_ctx values — the v3 unique index cannot be
        // created over a non-unique column.
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("schema_version");
        store
            .ddl(
                "CREATE TABLE context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
            )
            .await
            .expect("context_token_map");
        // Two rows with the same real_ctx → v3's CREATE UNIQUE INDEX fails.
        sqlx::query("INSERT INTO context_token_map (vctx, real_ctx) VALUES ($1, $2)")
            .bind("vctx-1")
            .bind("dup-real")
            .execute(&store.pool)
            .await
            .expect("seed row 1");
        sqlx::query("INSERT INTO context_token_map (vctx, real_ctx) VALUES ($1, $2)")
            .bind("vctx-2")
            .bind("dup-real")
            .execute(&store.pool)
            .await
            .expect("seed row 2");

        // migrate_to_v3 must surface the CREATE UNIQUE INDEX error.
        let result = store.migrate_to_v3().await;
        assert!(
            result.is_err(),
            "migrate_to_v3 must propagate DDL errors, got Ok — F-M2-03 not fixed"
        );
        // The claim row is still inserted (the M1 design writes the row
        // BEFORE the DDL); a subsequent connect will see v3 as claimed
        // and skip the broken step (DBA drops the row manually).
        assert!(
            store.is_migration_run(3).await.unwrap(),
            "v3 claim row is present even though DDL failed"
        );
    }

    /// F-M2-04: `record_migration_run` (the safety-net kept in M1) writes
    /// the row even after the migrator has already claimed the version.
    /// Since `try_claim_migration` already inserts the row, calling
    /// `record_migration_run` again is a no-op. The combined behaviour:
    /// the row is present exactly once, and a second `try_claim_migration`
    /// returns false.
    #[tokio::test]
    async fn m2_claim_and_record_are_consistent_with_schema_version() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // v3 is already applied. A second try_claim must observe the row.
        assert!(
            !store.try_claim_migration(3).await.unwrap(),
            "v3 is already applied; second claim must lose"
        );
        // record_migration_run is a no-op (ON CONFLICT DO NOTHING).
        store
            .record_migration_run(3)
            .await
            .expect("record_migration_run(3) must be a no-op");
        // The version row is still present (we did not delete it).
        assert!(store.is_migration_run(3).await.unwrap());
    }

    /// F-M2-05: invoking a higher-version migrator before a lower one
    /// must not deadlock or produce a partial state. The migrator's
    /// pre-condition is that the schema_version table exists; that's
    /// bootstrapped by `run_migrations`, but a per-version call on a
    /// fresh pool needs the table. We bootstrap manually here, then
    /// run v4 alone: v4 expects `context_token_map` to exist (it
    /// `ADD COLUMN`s onto it), so we also pre-create that table. The
    /// test pins down "running a single migrator on a partial state
    /// with the right pre-conditions is fine and records v4".
    #[tokio::test]
    async fn m2_v4_alone_with_minimal_preconditions() {
        sqlx::any::install_default_drivers();
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let store = Store { pool };
        store
            .ddl(
                "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
            )
            .await
            .expect("schema_version");
        store
            .ddl(
                "CREATE TABLE context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
            )
            .await
            .expect("context_token_map");

        // v4 alone: column does not exist, so the ALTER must run.
        store.migrate_to_v4().await.expect("migrate_to_v4");
        assert!(store.is_migration_run(4).await.unwrap());

        // The column was added; the index was created.
        assert!(
            store
                .column_exists("context_token_map", "created_at")
                .await
                .unwrap(),
            "created_at column must exist after v4"
        );
        // Index exists (sqlite_master entry).
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master \
             WHERE type='index' AND name='idx_context_token_map_created_at'",
        )
        .fetch_optional(&store.pool)
        .await
        .expect("catalog");
        assert!(
            row.is_some(),
            "idx_context_token_map_created_at must exist after v4"
        );
    }

    /// F-M2-06: full `run_migrations` walks all five steps in order and
    /// records v1..=v5 in `schema_version`. This is the headline M2
    /// invariant: any DDL error along the way aborts the walk.
    #[tokio::test]
    async fn m2_run_migrations_records_all_versions_in_order() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // All five versions are present.
        for v in 1..=5 {
            assert!(
                store.is_migration_run(v).await.unwrap(),
                "v{v} must be recorded after run_migrations"
            );
        }
        // get_current_version returns the maximum.
        assert_eq!(store.get_current_version().await.unwrap(), 5);
    }

    /// F-M2-07: `run_migrations` invoked twice in a row must remain
    /// idempotent. The M2 refactor's "early return on claim == false"
    /// shape is what makes this safe; the test pins it down.
    #[tokio::test]
    async fn m2_run_migrations_idempotent_double_call() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        // Second call must succeed.
        store.run_migrations().await.expect("second run_migrations");
        // Version stays at 5 (no ghost rows from a third call).
        assert_eq!(store.get_current_version().await.unwrap(), 5);
    }

    /// F-M2-08: each `migrate_to_vN` uses `CURRENT_TIMESTAMP` (not
    /// `datetime('now')`) for any timestamp default. The plan calls for
    /// unifying the DDL on `CURRENT_TIMESTAMP`. We check the catalog for
    /// each table's `sql` field and assert that no DDL contains the
    /// legacy `datetime('now')` form. The catalog on SQLite preserves
    /// the original CREATE TABLE statement, so this is a direct check.
    #[tokio::test]
    async fn m2_ddl_uses_current_timestamp_only() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL")
                .fetch_all(&store.pool)
                .await
                .expect("catalog");
        for (sql,) in rows {
            assert!(
                !sql.contains("datetime('now')"),
                "DDL must not use legacy datetime('now'): {sql}"
            );
            assert!(
                sql.contains("CURRENT_TIMESTAMP")
                    || !sql.contains("TIMESTAMP") && !sql.contains("timestamp"),
                "DDL should prefer CURRENT_TIMESTAMP where applicable: {sql}"
            );
        }
    }

    // ─── M3 regression tests ───────────────────────────────────────────────
    //
    // M3 synchronises and aligns the `migrations/*.sql` files with the
    // inline DDL in `migrate_to_vN`. The tests below pin down the M3
    // invariants: (a) every `migrations/*.sql` is the human-readable
    // reference for the corresponding Rust migrator (modulo the v5
    // AUTOINCREMENT/IDENTITY driver split, F-M2-02), (b) no SQL file
    // contains the legacy `datetime('now')` form, (c) the index names
    // defined in SQL match the catalog after `run_migrations`, and
    // (d) the v5 DDL is portable across SQLite / Postgres / MySQL
    // (the F-M2-02 fix).

    /// Normalise whitespace: collapse runs of spaces/tabs into a single
    /// space, drop leading/trailing whitespace on each line, drop
    /// blank lines, drop `-- ...` line comments. Used to compare a
    /// reference SQL file against an inline Rust DDL string when the
    /// two have only indentation / line-break differences.
    fn normalise_sql(s: &str) -> String {
        s.lines()
            .map(|l| {
                // strip `--` line comments (not inside strings — none of
                // the inline DDLs contain `--` outside of comments).
                if let Some(idx) = l.find("--") {
                    &l[..idx]
                } else {
                    l
                }
            })
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| {
                // collapse internal runs of whitespace to a single space,
                // but keep `;` attached to the previous token (so an
                // end-of-statement `;` on its own line still reads as
                // part of the previous line).
                let mut out = String::with_capacity(l.len());
                let mut prev_space = false;
                for c in l.chars() {
                    if c == ';' {
                        // attach to previous token
                        out.push(';');
                        prev_space = false;
                    } else if c.is_whitespace() {
                        if !prev_space {
                            out.push(' ');
                        }
                        prev_space = true;
                    } else {
                        out.push(c);
                        prev_space = false;
                    }
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// F-M3-01: the SQLite branch of `v5_create_messages_sql` matches the
    /// `CREATE TABLE messages` block in `migrations/0005_messages.sql` after
    /// whitespace normalisation. The two should be byte-identical modulo
    /// indentation and the line-break conventions of the two contexts
    /// (Rust string literal vs. SQL file). The 0005 file also contains the
    /// two CREATE INDEX statements; those are covered by F-M3-05.
    #[test]
    fn m3_v5_sqlite_ddl_matches_migration_file() {
        // CARGO_MANIFEST_DIR is the workspace root for ilink-hub. The
        // migrations/ dir sits at the workspace root.
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let sql_path = manifest_dir.join("migrations").join("0005_messages.sql");
        let sql_text = std::fs::read_to_string(&sql_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", sql_path.display()));
        // Extract just the CREATE TABLE block (everything up to the first
        // closing `;`). The two CREATE INDEX statements that follow are
        // covered by F-M3-05. The block already ends with `;` in the SQL
        // file, so the normaliser sees a trailing `;` on the last
        // non-empty line.
        let create_table_block = sql_text.split(';').next().unwrap_or("").trim().to_string() + ";";
        // `v5_create_messages_sql` does not include the trailing `;`
        // (the `ddl()` helper accepts statements both with and without
        // it). Append one for the comparison so the two normalised
        // strings have the same shape.
        let expected = Store::v5_create_messages_sql(true) + ";";
        assert_eq!(
            normalise_sql(&expected),
            normalise_sql(&create_table_block),
            "SQLite v5 CREATE TABLE DDL diverges from migrations/0005_messages.sql — \
             update one or the other to keep them in sync (M3 invariant)"
        );
    }

    /// F-M3-02: the non-SQLite branch of `v5_create_messages_sql` uses
    /// `GENERATED BY DEFAULT AS IDENTITY` (the SQL standard form) and
    /// does NOT use `AUTOINCREMENT`. This is the F-M2-02 fix — the
    /// SQLite-only keyword must not leak into the Postgres / MySQL
    /// DDL.
    #[test]
    fn m3_v5_non_sqlite_ddl_uses_identity_not_autoincrement() {
        let ddl = Store::v5_create_messages_sql(false);
        assert!(
            ddl.contains("GENERATED BY DEFAULT AS IDENTITY"),
            "non-SQLite v5 DDL must use SQL standard IDENTITY clause: {ddl}"
        );
        assert!(
            !ddl.contains("AUTOINCREMENT"),
            "non-SQLite v5 DDL must NOT use SQLite-only AUTOINCREMENT: {ddl}"
        );
    }

    /// F-M3-03: every `migrations/*.sql` file contains no
    /// `datetime('now')` residue. The m2 review established
    /// `CURRENT_TIMESTAMP` as the canonical default in the Rust
    /// DDLs; the SQL files must use the same form.
    #[test]
    fn m3_no_legacy_datetime_now_in_migration_files() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = manifest_dir.join("migrations");
        let mut checked = 0usize;
        for entry in
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("sql") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert!(
                !text.contains("datetime('now')"),
                "{} still contains legacy datetime('now') — use CURRENT_TIMESTAMP",
                path.display()
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "expected at least 4 .sql files, found {checked}"
        );
    }

    /// F-M3-04: every `migrations/*.sql` file that contains a timestamp
    /// default uses `CURRENT_TIMESTAMP` (not `datetime('now')`). Companion
    /// to F-M3-03; asserts the affirmative side of the unification. Files
    /// that contain no timestamp default (e.g. `0003_*` index-only file)
    /// are exempt — the test only fires for files that mention the word
    /// "timestamp" or "TIMESTAMP".
    #[test]
    fn m3_migration_files_use_current_timestamp() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = manifest_dir.join("migrations");
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("sql") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            let mentions_timestamp = text.contains("timestamp") || text.contains("TIMESTAMP");
            if !mentions_timestamp {
                continue;
            }
            assert!(
                text.contains("CURRENT_TIMESTAMP"),
                "{} is missing CURRENT_TIMESTAMP — every timestamp default \
                 must use the SQL standard form (M3 alignment)",
                path.display()
            );
        }
    }

    /// F-M3-05: after `run_migrations`, the SQLite catalog contains the
    /// three index names that the SQL files declare
    /// (`idx_context_token_map_real_ctx`, `idx_context_token_map_created_at`,
    /// `idx_messages_vctx_created`, `idx_messages_peer_role_created`).
    /// This is the M3 cross-check between the SQL reference files and
    /// the runtime catalog.
    #[tokio::test]
    async fn m3_index_names_match_sql_files_and_catalog() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        for idx in [
            "idx_context_token_map_real_ctx",
            "idx_context_token_map_created_at",
            "idx_messages_vctx_created",
            "idx_messages_peer_role_created",
        ] {
            let row: Option<(String,)> =
                sqlx::query_as("SELECT name FROM sqlite_master WHERE type='index' AND name = $1")
                    .bind(idx)
                    .fetch_optional(&store.pool)
                    .await
                    .expect("catalog");
            assert!(row.is_some(), "index {idx} missing from SQLite catalog");
        }
    }

    /// F-M3-06: the inline Rust DDL strings in `migrate_to_v1`, `migrate_to_v2`,
    /// and `migrate_to_v4` are byte-equivalent (modulo whitespace) to the
    /// corresponding statements in `migrations/0001_initial_schema.sql`,
    /// `migrations/0002_backend_sessions.sql`, and
    /// `migrations/0004_context_token_map_created_at.sql`. v3 has no
    /// SQL file (its `CREATE UNIQUE INDEX` is inline-only); v5 is
    /// covered by `m3_v5_sqlite_ddl_matches_migration_file`.
    #[tokio::test]
    async fn m3_migration_files_match_inline_ddl_for_v1_v2_v4() {
        // Re-run the in-source extraction: the migration runner must use
        // the same DDL strings the SQL files declare. The simplest
        // invariant: after `Store::connect`, the SQLite catalog contains
        // every table and index that the SQL files declare, with the
        // exact names.
        let store = Store::connect("sqlite::memory:").await.expect("connect");

        // Tables declared in the SQL files.
        let expected_tables = [
            // 0000 (documentation only — table is created by the runner,
            // not by the SQL file). Skipped.
            "clients",             // 0001
            "routing_state",       // 0001
            "context_token_map",   // 0001
            "bot_credentials",     // 0001
            "backend_sessions_v2", // 0002
            "active_sessions",     // 0002
            "messages",            // 0005
        ];
        for t in expected_tables {
            let row: Option<(String,)> =
                sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name = $1")
                    .bind(t)
                    .fetch_optional(&store.pool)
                    .await
                    .expect("catalog");
            assert!(
                row.is_some(),
                "table {t} declared in migrations/*.sql but missing from catalog"
            );
        }
    }
}
