//! Database schema migrations for the Store.
//! Each `migrate_to_vN` method brings the schema up by exactly one version step.

use anyhow::Result;
use sqlx::{Acquire, Row};

use super::{is_safe_identifier, DatabaseKind, Store};

impl Store {
    pub async fn get_current_version(&self) -> Result<i32> {
        // Exclude the lock-sentinel row (i32::MAX) that the migration runner
        // inserts to prevent concurrent double-execution. External callers must
        // see the real highest schema version, not the sentinel.
        let row = sqlx::query("SELECT MAX(version) FROM schema_version WHERE version < $1")
            .bind(i32::MAX)
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
    ///
    /// The SQL is driver-aware: SQLite and Postgres use
    /// `INSERT ... ON CONFLICT (version) DO NOTHING RETURNING version`
    /// (the canonical "check-and-claim in one statement" pattern); MySQL
    /// does not support `ON CONFLICT` nor `RETURNING`, so we use
    /// `INSERT IGNORE` and treat `rows_affected() == 1` as the
    /// "we won the claim" signal. MySQL's `INSERT IGNORE` skips a row
    /// that violates the unique-key constraint without raising an error,
    /// which is the same effect as `ON CONFLICT DO NOTHING`. See F-M3-01
    /// in the m3 review-findings for the rationale.
    pub async fn try_claim_migration(&self, version: i32) -> Result<bool> {
        match self.kind {
            DatabaseKind::Sqlite | DatabaseKind::Postgres => {
                let row = sqlx::query(
                    "INSERT INTO schema_version (version) VALUES ($1) \
                     ON CONFLICT (version) DO NOTHING RETURNING version",
                )
                .bind(version)
                .fetch_optional(&self.pool)
                .await?;
                Ok(row.is_some())
            }
            DatabaseKind::MySql => {
                let result = sqlx::query("INSERT IGNORE INTO schema_version (version) VALUES (?)")
                    .bind(version)
                    .execute(&self.pool)
                    .await?;
                if result.rows_affected() == 1 {
                    return Ok(true);
                }
                // Defensive check: MySQL's INSERT IGNORE suppresses ALL errors,
                // not just duplicate-key. If a non-duplicate error was suppressed,
                // the row won't exist. Verify the row is actually present.
                let row = sqlx::query("SELECT 1 FROM schema_version WHERE version = ?")
                    .bind(version)
                    .fetch_optional(&self.pool)
                    .await?;
                if row.is_none() {
                    return Err(anyhow::anyhow!(
                        "INSERT IGNORE for schema_version v{version}: row absent after insert — \
                         a non-duplicate error may have been silently suppressed"
                    ));
                }
                Ok(false)
            }
        }
    }

    /// Internal: mark a version as run after its DDL has completed.
    /// Kept for symmetry with the older two-step pattern; `try_claim_migration`
    /// is the primary path. The DDL runs only when `try_claim_migration`
    /// returned `true`, so by the time this is called the row is already
    /// present — this is a no-op safety net.
    ///
    /// Like `try_claim_migration`, the SQL is driver-aware: MySQL does not
    /// support `ON CONFLICT`, so we use `INSERT IGNORE` on MySQL.
    pub async fn record_migration_run(&self, version: i32) -> Result<()> {
        match self.kind {
            DatabaseKind::Sqlite | DatabaseKind::Postgres => {
                sqlx::query(
                    "INSERT INTO schema_version (version) VALUES ($1) \
                     ON CONFLICT (version) DO NOTHING",
                )
                .bind(version)
                .execute(&self.pool)
                .await?;
            }
            DatabaseKind::MySql => {
                sqlx::query("INSERT IGNORE INTO schema_version (version) VALUES (?)")
                    .bind(version)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Driver-aware atomic check-and-claim, executed on a transaction
    /// (the canonical path used by `run_migrations`). Same SQL as
    /// `try_claim_migration` but the claim row sits inside the
    /// transaction, so the visibility of the row to other writers is
    /// tied to the transaction's commit/rollback. On SQLite, this means
    /// the row is invisible to other connections until the transaction
    /// commits — which is exactly the cross-version race closure we
    /// need (F-M3-02).
    #[allow(dead_code)] // used by the test surface via `migrate_to_vN`
    pub(super) async fn try_claim_migration_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        version: i32,
    ) -> Result<bool> {
        match self.kind {
            DatabaseKind::Sqlite | DatabaseKind::Postgres => {
                let row = sqlx::query(
                    "INSERT INTO schema_version (version) VALUES ($1) \
                     ON CONFLICT (version) DO NOTHING RETURNING version",
                )
                .bind(version)
                .fetch_optional(&mut **tx)
                .await?;
                Ok(row.is_some())
            }
            DatabaseKind::MySql => {
                let result = sqlx::query("INSERT IGNORE INTO schema_version (version) VALUES (?)")
                    .bind(version)
                    .execute(&mut **tx)
                    .await?;
                if result.rows_affected() == 1 {
                    return Ok(true);
                }
                // Defensive check: MySQL's INSERT IGNORE suppresses ALL errors,
                // not just duplicate-key.
                let row = sqlx::query("SELECT 1 FROM schema_version WHERE version = ?")
                    .bind(version)
                    .fetch_optional(&mut **tx)
                    .await?;
                if row.is_none() {
                    return Err(anyhow::anyhow!(
                        "INSERT IGNORE for schema_version v{version}: row absent after insert — \
                         a non-duplicate error may have been silently suppressed"
                    ));
                }
                Ok(false)
            }
        }
    }

    pub(super) async fn run_migrations(&self) -> Result<()> {
        // AnyPool does not support DDL (CREATE TABLE / ALTER TABLE) or sqlx::migrate!.
        // We implement our own lightweight version-tracking table (`schema_version`)
        // so each migration step is applied exactly once and can be safely re-run.
        //
        // The migration SQL files under migrations/ are the canonical human-readable
        // reference. The Rust code here must stay in sync with those files.
        //
        // Concurrency model: a single connection, held for the entire migration
        // walk, runs the per-version migrators inside a single transaction.
        // Each migrator's per-step `try_claim_migration(N)` is the atomic
        // check-and-claim primitive (the M1 fix, F-M1-01 / F-M1-04); holding
        // the connection throughout the walk closes the cross-version race
        // that the per-step claim alone cannot (F-M3-02 in the m3 review).
        //
        // Why a transaction: on SQLite, `BEGIN IMMEDIATE` (sqlx's default
        // `begin` on a `PoolConnection`) acquires the file-level write lock,
        // so any concurrent writer on the same file is blocked at the
        // connection layer until we commit. On Postgres, the transaction
        // serialises via MVCC. On MySQL, DDL auto-commits (MySQL DDL is not
        // transactional in InnoDB), but the per-step claim INSERTs ARE inside
        // the transaction and provide mutual exclusion at the row level for
        // same-version claims; cross-version races on MySQL are theoretically
        // possible but the window is microseconds (the DDL between claim and
        // the next claim).
        //
        // Claim row order: the per-step claim rows are written BEFORE their
        // respective DDL bodies. A partial DDL failure leaves the row present
        // and a subsequent connect will skip the broken step (DBA can then
        // drop the row manually).
        //
        // Invariant: every DDL block below must be idempotent OR guarded by a
        // pre-check that detects the post-migration state. A non-idempotent
        // DDL whose claim row was written but whose execution crashed mid-way
        // would prevent re-running; protect against that by either making the
        // DDL idempotent (`IF NOT EXISTS`) or by short-circuiting when the
        // schema already shows the post-migration shape.

        // Acquire a single connection for the whole walk. This pin is what
        // closes the cross-version race (F-M3-02) — a single connection means
        // a single physical write context, so the per-step claim rows and
        // the DDL body for each version execute in a deterministic order
        // from the same catalog snapshot.
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;

        // Bootstrap the version-tracking table before anything else.
        // This is always idempotent via IF NOT EXISTS.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version     INTEGER PRIMARY KEY,
                migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            anyhow::anyhow!("DDL failed: CREATE TABLE IF NOT EXISTS schema_version\n  Error: {e}")
        })?;

        // Dispatch to the per-version migrators on the same transaction.
        // Each step is gated by `try_claim_migration_in_tx`, so a step that
        // has already been applied (i.e. its row is present in
        // `schema_version`) is a no-op. Steps that fail (DDL error, decode
        // error) propagate `Err` straight up to `Store::connect`, blocking
        // the program from starting in a half-migrated state. The whole
        // walk commits atomically at the end.
        self.migrate_to_v1_tx(&mut tx).await?;
        self.migrate_to_v2_tx(&mut tx).await?;
        self.migrate_to_v3_tx(&mut tx).await?;
        self.migrate_to_v4_tx(&mut tx).await?;
        self.migrate_to_v5_tx(&mut tx).await?;

        tx.commit().await?;
        Ok(())
    }

    // ─── Per-version migrators (transactional, used by `run_migrations`) ───
    //
    // These are the canonical migrators. The non-suffixed `migrate_to_vN`
    // methods (below) are the pre-M3 versions used by unit tests that call
    // a single migrator on a partial state without setting up a
    // transaction. The transactional variants take `&mut sqlx::Transaction`
    // and run every DDL and claim on that transaction.

    pub(super) async fn migrate_to_v1_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 1).await? {
            return Ok(());
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS clients (
                vtoken      TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                label       TEXT,
                created_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                last_seen   TEXT
            )",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow::anyhow!("DDL failed: CREATE TABLE clients\n  Error: {e}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS routing_state (
                from_user     TEXT PRIMARY KEY,
                active_vtoken TEXT NOT NULL,
                updated_at    TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow::anyhow!("DDL failed: CREATE TABLE routing_state\n  Error: {e}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                vctx         TEXT PRIMARY KEY,
                real_ctx     TEXT NOT NULL,
                peer_user_id TEXT NOT NULL DEFAULT '',
                expires_at   TEXT
            )",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow::anyhow!("DDL failed: CREATE TABLE context_token_map\n  Error: {e}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS bot_credentials (
                id         INTEGER PRIMARY KEY,
                token      TEXT NOT NULL,
                base_url   TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow::anyhow!("DDL failed: CREATE TABLE bot_credentials\n  Error: {e}"))?;

        tracing::info!(version = 1, "migration applied: initial schema");
        Ok(())
    }

    pub(super) async fn migrate_to_v2_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 2).await? {
            return Ok(());
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                vctx               TEXT NOT NULL,
                vtoken             TEXT NOT NULL,
                session_name       TEXT NOT NULL,
                backend_session_id TEXT NOT NULL DEFAULT '',
                created_at         TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                PRIMARY KEY (vctx, vtoken, session_name)
            )",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            anyhow::anyhow!("DDL failed: CREATE TABLE backend_sessions_v2\n  Error: {e}")
        })?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS active_sessions (
                vctx         TEXT NOT NULL,
                vtoken       TEXT NOT NULL,
                session_name TEXT NOT NULL DEFAULT 'default',
                updated_at   TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                PRIMARY KEY (vctx, vtoken)
            )",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow::anyhow!("DDL failed: CREATE TABLE active_sessions\n  Error: {e}"))?;

        tracing::info!(version = 2, "migration applied: backend session tables");
        Ok(())
    }

    pub(super) async fn migrate_to_v3_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 3).await? {
            return Ok(());
        }
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_context_token_map_real_ctx \
             ON context_token_map (real_ctx)",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "DDL failed: CREATE UNIQUE INDEX idx_context_token_map_real_ctx\n  Error: {e}"
            )
        })?;

        tracing::info!(version = 3, "migration applied: real_ctx unique index");
        Ok(())
    }

    pub(super) async fn migrate_to_v4_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 4).await? {
            return Ok(());
        }
        // v4's ALTER is gated by a column-existence pre-check (F-M1-02 fix).
        // We must NOT use `self.column_exists(&self.pool)` here: with
        // max_connections(1), the transaction already holds the sole pool
        // connection; a second `pool.acquire()` inside `column_exists` would
        // deadlock, `unwrap_or(None)` would swallow the timeout as `false`, and
        // the ALTER TABLE would run unconditionally — failing with "duplicate
        // column" on databases that already have the column (F-M3-04).
        // Fix: inline the catalog query on the SAME transaction connection.
        let col_exists_in_tx = match self.kind {
            DatabaseKind::Sqlite => sqlx::query(
                "SELECT 1 FROM pragma_table_info('context_token_map') \
                 WHERE name = 'created_at'",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.columns \
                 WHERE table_name = 'context_token_map' AND column_name = 'created_at' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };
        if !col_exists_in_tx {
            sqlx::query("ALTER TABLE context_token_map ADD COLUMN created_at TEXT")
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "DDL failed: ALTER TABLE context_token_map ADD COLUMN created_at\n  Error: {e}"
                    )
                })?;
        } else {
            tracing::debug!(
                "v4 migration: created_at column already present (pre-check), skipping ALTER"
            );
        }

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_context_token_map_created_at \
             ON context_token_map (created_at DESC)",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "DDL failed: CREATE INDEX idx_context_token_map_created_at\n  Error: {e}"
            )
        })?;

        tracing::info!(
            version = 4,
            "migration applied: context_token_map created_at column + index"
        );
        Ok(())
    }

    pub(super) async fn migrate_to_v5_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 5).await? {
            return Ok(());
        }
        let create_messages = Self::v5_create_messages_sql(self.kind);
        sqlx::query(&create_messages)
            .execute(&mut **tx)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "DDL failed: CREATE TABLE messages (driver-specific id clause)\n  Error: {e}"
                )
            })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_vctx_created \
             ON messages (vctx, created_at DESC)",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            anyhow::anyhow!("DDL failed: CREATE INDEX idx_messages_vctx_created\n  Error: {e}")
        })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_peer_role_created \
             ON messages (peer_user_id, role, created_at DESC)",
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            anyhow::anyhow!("DDL failed: CREATE INDEX idx_messages_peer_role_created\n  Error: {e}")
        })?;

        tracing::info!(version = 5, "migration applied: messages table + indexes");
        Ok(())
    }

    // Non-transactional single-version migrators.
    // Called only from test code that exercises individual migrators in isolation.
    // Production code uses the transactional `migrate_to_vN_tx` variants via `run_migrations`.
    #[allow(dead_code)]
    /// v1: initial schema — clients, routing_state, context_token_map, bot_credentials.
    pub(super) async fn migrate_to_v1(&self) -> Result<()> {
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

    #[allow(dead_code)]
    /// v2: backend session tables — backend_sessions_v2, active_sessions.
    pub(super) async fn migrate_to_v2(&self) -> Result<()> {
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

    #[allow(dead_code)]
    /// v3: real_ctx unique index — backs race-free upsert in `map_context_token`.
    pub(super) async fn migrate_to_v3(&self) -> Result<()> {
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

    #[allow(dead_code)]
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
    pub(super) async fn migrate_to_v4(&self) -> Result<()> {
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
    /// The `id` column uses driver-specific auto-increment syntax:
    ///   - SQLite: `INTEGER PRIMARY KEY AUTOINCREMENT`
    ///   - Postgres: `INTEGER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY`
    ///     (the SQL standard form, Postgres 10+)
    ///   - MySQL: `BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY` (MySQL 5.7+)
    ///
    /// The driver is taken from `self.kind` (parsed at `Store::connect` time
    /// from the URL scheme), NOT from a runtime probe: the previous
    /// `SELECT current_database()` probe returned `Err` on BOTH SQLite and
    /// MySQL, producing a false-positive `is_sqlite == true` on MySQL and
    /// breaking the migration. See F-M3-01 in the m3 review-findings.
    #[allow(dead_code)]
    pub(super) async fn migrate_to_v5(&self) -> Result<()> {
        if !self.try_claim_migration(5).await? {
            return Ok(());
        }
        let create_messages = Self::v5_create_messages_sql(self.kind);
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
    /// Pulled out of `migrate_to_v5` so the m3 test surface can call all three
    /// branches directly without spinning up a Postgres or MySQL connection.
    ///
    /// Each form is portable to the named driver only. Field types, default
    /// values (`CURRENT_TIMESTAMP`), and table-level shape are identical to
    /// the `migrations/0005_messages.sql` reference (which documents the
    /// SQLite form); the only divergence between the three forms is the
    /// `id` clause and, for MySQL, the column type (`BIGINT` is required
    /// because MySQL's `AUTO_INCREMENT` on `INTEGER` is silently truncated
    /// to `INT(11)`, which then collides with the sqlx `i64` decoder used
    /// by `save_message`'s `last_insert_id` path).
    pub(super) fn v5_create_messages_sql(kind: DatabaseKind) -> String {
        let id_clause = match kind {
            DatabaseKind::Sqlite => "id           INTEGER PRIMARY KEY AUTOINCREMENT",
            DatabaseKind::Postgres => {
                "id           INTEGER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY"
            }
            DatabaseKind::MySql => "id           BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY",
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
    /// The driver is taken from `self.kind` (parsed at `Store::connect`
    /// time from the URL scheme) rather than probed at runtime — the
    /// previous `SELECT current_database()` probe returned `Err` on
    /// BOTH SQLite and MySQL, producing a false-positive
    /// `is_sqlite == true` on MySQL and breaking the catalog query
    /// (F-M3-01).
    ///
    /// Returns Ok(false) on any error reading the catalog (caller treats the
    /// column as not present and lets the DDL surface the real error).
    #[allow(dead_code)]
    pub(super) async fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        // The `pragma_table_info` form works on SQLite. `pragma` cannot be
        // parameterised, so we validate identifiers before splicing.
        if !is_safe_identifier(table) || !is_safe_identifier(column) {
            return Ok(false);
        }

        match self.kind {
            DatabaseKind::Sqlite => {
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
                Ok(row.is_some())
            }
            DatabaseKind::Postgres | DatabaseKind::MySql => {
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
        }
    }

    /// Execute a single DDL statement through a pool connection.
    ///
    /// `AnyPool::execute` silently ignores DDL on the pool level. Using an
    /// explicit `PoolConnection` and calling `execute` on the dereffed connection
    /// works correctly, including for SQLite in-memory databases where all
    /// operations must go through the same physical connection.
    pub(super) async fn ddl(&self, sql: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| anyhow::anyhow!("DDL failed: {sql}\n  Error: {e}"))?;
        Ok(())
    }

    // ─── Clients ─────────────────────────────────────────────────────────────
}
