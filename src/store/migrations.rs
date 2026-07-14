//! Schema version tracking and migrations.
//!
//! Split out of `mod.rs`. The canonical transactional migrators
//! (`migrate_to_vN_tx`) run inside `run_migrations`; the non-`_tx`
//! variants are thin wrappers that open their own transaction and delegate,
//! used only by unit tests that exercise individual migrators in isolation.

use anyhow::Result;
use sqlx::{Acquire, Row};

use super::{DatabaseKind, Store};

/// Whitelist check used by SQLite `pragma_table_info` splicing: the table
/// and column names must contain only identifier characters, so the
/// interpolated string cannot smuggle SQL.
///
/// Allowed in non-test builds because the only caller (`column_exists`)
/// is itself a test-only helper; the `cfg_attr(not(test), allow(...))`
/// keeps the production build warning-clean while still letting clippy
/// flag a real dead-code regression in the test build.
#[cfg_attr(not(test), allow(dead_code))]
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Store {
    /// Returns the highest version recorded in `schema_version`, or 0 if the
    /// table is empty. Decode errors are propagated — a DB that has rows but
    /// fails to decode them is NOT the same as a fresh DB.
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
        // SQL for each version lives in migrations/000N_*.sql and is embedded at
        // compile time via include_str!. The Rust code here handles driver-specific
        // branching that cannot be expressed in a single portable SQL file.
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

        self.migrate_to_v1_tx(&mut tx).await?;
        self.migrate_to_v2_tx(&mut tx).await?;
        self.migrate_to_v3_tx(&mut tx).await?;
        self.migrate_to_v4_tx(&mut tx).await?;
        self.migrate_to_v5_tx(&mut tx).await?;
        self.migrate_to_v6_tx(&mut tx).await?;
        self.migrate_to_v7_tx(&mut tx).await?;
        self.migrate_to_v8_tx(&mut tx).await?;
        self.migrate_to_v9_tx(&mut tx).await?;
        self.migrate_to_v10_tx(&mut tx).await?;
        self.migrate_to_v11_tx(&mut tx).await?;
        self.migrate_to_v12_tx(&mut tx).await?;
        self.migrate_to_v13_tx(&mut tx).await?;
        self.migrate_to_v14_tx(&mut tx).await?;

        tx.commit().await?;
        Ok(())
    }

    // ─── Per-version migrators (transactional, used by `run_migrations`) ───

    pub(super) async fn migrate_to_v1_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 1).await? {
            return Ok(());
        }
        for sql in V1_DDLS {
            sqlx::query(sql)
                .execute(&mut **tx)
                .await
                .map_err(|e| anyhow::anyhow!("DDL failed: {sql}\n  Error: {e}"))?;
        }
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
        for sql in V2_DDLS {
            sqlx::query(sql)
                .execute(&mut **tx)
                .await
                .map_err(|e| anyhow::anyhow!("DDL failed: {sql}\n  Error: {e}"))?;
        }
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
        sqlx::query(V3_CREATE_IDX)
            .execute(&mut **tx)
            .await
            .map_err(|e| anyhow::anyhow!("DDL failed: {V3_CREATE_IDX}\n  Error: {e}"))?;
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
            sqlx::query(V4_ALTER_ADD_CREATED_AT)
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("DDL failed: {V4_ALTER_ADD_CREATED_AT}\n  Error: {e}")
                })?;
        } else {
            tracing::debug!(
                "v4 migration: created_at column already present (pre-check), skipping ALTER"
            );
        }
        sqlx::query(V4_CREATE_IDX)
            .execute(&mut **tx)
            .await
            .map_err(|e| anyhow::anyhow!("DDL failed: {V4_CREATE_IDX}\n  Error: {e}"))?;
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
            .map_err(|e| anyhow::anyhow!("DDL failed (CREATE TABLE messages): {e}"))?;
        for sql in V5_INDEXES {
            sqlx::query(sql)
                .execute(&mut **tx)
                .await
                .map_err(|e| anyhow::anyhow!("DDL failed: {sql}\n  Error: {e}"))?;
        }
        tracing::info!(version = 5, "migration applied: messages table + indexes");
        Ok(())
    }

    pub(super) async fn migrate_to_v6_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 6).await? {
            return Ok(());
        }
        sqlx::query(V6_NORMALIZE_PEER_USER_ID)
            .execute(&mut **tx)
            .await
            .map_err(|e| {
                anyhow::anyhow!("DDL failed: {V6_NORMALIZE_PEER_USER_ID}\n  Error: {e}")
            })?;
        tracing::info!(
            version = 6,
            "migration applied: normalize context_token_map.peer_user_id to peer:/group: form"
        );
        Ok(())
    }

    /// v7: Unique index on non-empty `peer_user_id` in `context_token_map`.
    ///
    /// SQLite and PostgreSQL support partial WHERE clauses on the index;
    /// MySQL does not support partial indexes and is skipped here — the
    /// serialised single-connection write pool provides equivalent protection.
    ///
    /// De-duplication of historical rows runs before the index is created,
    /// using driver-specific SQL (SQLite: rowid, PostgreSQL: ctid) that
    /// cannot be expressed portably in a single SQL file.
    pub(super) async fn migrate_to_v7_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 7).await? {
            return Ok(());
        }
        match self.kind {
            DatabaseKind::Sqlite | DatabaseKind::Postgres => {
                let dedup_sql = match self.kind {
                    DatabaseKind::Sqlite => V7_DEDUP_PEER_USER_ID_SQLITE,
                    _ => V7_DEDUP_PEER_USER_ID_POSTGRES,
                };
                sqlx::query(dedup_sql)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("DDL failed: de-duplicate peer_user_id\n  Error: {e}")
                    })?;
                sqlx::query(V7_UNIQUE_IDX_PEER_USER_ID)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("DDL failed: {V7_UNIQUE_IDX_PEER_USER_ID}\n  Error: {e}")
                    })?;
                tracing::info!(
                    version = 7,
                    "migration applied: de-dup + unique index on non-empty peer_user_id"
                );
            }
            DatabaseKind::MySql => {
                tracing::info!(
                    version = 7,
                    "migration skipped on MySQL: partial unique indexes are not supported; \
                     find_or_create_vctx uses serialised pool instead"
                );
            }
        }
        Ok(())
    }

    pub(super) async fn migrate_to_v8_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 8).await? {
            return Ok(());
        }

        // Detection logic: check if the first client vtoken (if any) is already a 64-char hex.
        // If so, we assume the migration has already run and skip. We also guard against
        // the `clients` table being absent (e.g. test environments that exercise isolated
        // migration steps on a partial schema); if the table doesn't exist, treat it as
        // "nothing to migrate".
        let first_vtoken_row = sqlx::query("SELECT vtoken FROM clients LIMIT 1")
            .fetch_optional(&mut **tx)
            .await
            .or_else(|e| {
                let msg = e.to_string();
                if msg.contains("no such table") || msg.contains("doesn't exist") {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;

        if let Some(row) = first_vtoken_row {
            let vtoken: String = row.try_get(0)?;
            if crate::hub::is_vtoken_hash(&vtoken) {
                tracing::debug!(
                    "v8 migration: first client vtoken is already hashed, skipping data conversion"
                );
                return Ok(());
            }
        } else {
            // No clients exist yet — nothing to hash or encrypt. Still execute the
            // no-op DDL wrapper so the migration version is recorded consistently,
            // but skip the master-key requirement (the key is only needed when
            // actual plain-text credentials must be transformed).
            sqlx::query(V8_DDL)
                .execute(&mut **tx)
                .await
                .map_err(|e| anyhow::anyhow!("DDL failed: {V8_DDL}\n  Error: {e}"))?;
            return Ok(());
        }

        // Since we are going to migrate, the master key must be present/valid.
        let local_key: Option<crate::runtime::crypto::Key>;
        let master_key: &ring::aead::LessSafeKey = if let Some(k) = self.master_key() {
            k.as_ref()
        } else {
            match crate::runtime::crypto::load_or_derive_master_key() {
                Ok(k) => {
                    local_key = Some(k);
                    local_key.as_ref().expect("just assigned above")
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Migration v8 failed: ILINK_HUB_MASTER_KEY is required to encrypt bot credentials, but could not be loaded: {e}"
                    ));
                }
            }
        };

        // Run the no-op SQL wrapper for record keeping/schema integrity.
        sqlx::query(V8_DDL)
            .execute(&mut **tx)
            .await
            .map_err(|e| anyhow::anyhow!("DDL failed: {V8_DDL}\n  Error: {e}"))?;

        // 1. Migrate vtokens in clients, routing_state, and messages tables
        let mut vtokens_to_hash = std::collections::HashSet::new();

        // Collect from clients
        let clients_exist = match sqlx::query("SELECT vtoken FROM clients")
            .fetch_all(&mut **tx)
            .await
        {
            Ok(rows) => {
                for r in rows {
                    let vt: String = r.try_get(0)?;
                    if !crate::hub::is_vtoken_hash(&vt) {
                        vtokens_to_hash.insert(vt);
                    }
                }
                true
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("no such table") || msg.contains("doesn't exist") {
                    false
                } else {
                    return Err(e.into());
                }
            }
        };

        // Collect from routing_state
        if clients_exist {
            match sqlx::query("SELECT active_vtoken FROM routing_state")
                .fetch_all(&mut **tx)
                .await
            {
                Ok(rows) => {
                    for r in rows {
                        let vt: String = r.try_get(0)?;
                        if !crate::hub::is_vtoken_hash(&vt) {
                            vtokens_to_hash.insert(vt);
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("no such table") && !msg.contains("doesn't exist") {
                        return Err(e.into());
                    }
                }
            }

            // Collect from messages
            match sqlx::query("SELECT DISTINCT vtoken FROM messages WHERE vtoken IS NOT NULL")
                .fetch_all(&mut **tx)
                .await
            {
                Ok(rows) => {
                    for r in rows {
                        let vt: String = r.try_get(0)?;
                        if !crate::hub::is_vtoken_hash(&vt) {
                            vtokens_to_hash.insert(vt);
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("no such table") && !msg.contains("doesn't exist") {
                        return Err(e.into());
                    }
                }
            }
        }

        for old_vtoken in vtokens_to_hash {
            let new_vtoken = crate::hub::hash_vtoken(&old_vtoken);

            if clients_exist {
                sqlx::query("UPDATE clients SET vtoken = $1 WHERE vtoken = $2")
                    .bind(&new_vtoken)
                    .bind(&old_vtoken)
                    .execute(&mut **tx)
                    .await?;

                match sqlx::query(
                    "UPDATE routing_state SET active_vtoken = $1 WHERE active_vtoken = $2",
                )
                .bind(&new_vtoken)
                .bind(&old_vtoken)
                .execute(&mut **tx)
                .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        if !msg.contains("no such table") && !msg.contains("doesn't exist") {
                            return Err(e.into());
                        }
                    }
                }

                match sqlx::query("UPDATE messages SET vtoken = $1 WHERE vtoken = $2")
                    .bind(&new_vtoken)
                    .bind(&old_vtoken)
                    .execute(&mut **tx)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        if !msg.contains("no such table") && !msg.contains("doesn't exist") {
                            return Err(e.into());
                        }
                    }
                }
            }
        }

        // 2. Migrate bot_credentials table
        let cred_rows = match sqlx::query("SELECT id, token FROM bot_credentials")
            .fetch_all(&mut **tx)
            .await
        {
            Ok(rows) => Some(rows),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("no such table") || msg.contains("doesn't exist") {
                    None
                } else {
                    return Err(e.into());
                }
            }
        };

        if let Some(rows) = cred_rows {
            for row in rows {
                let id: i64 = row.try_get(0)?;
                let token: String = row.try_get(1)?;
                // Check if the token can be decrypted with the master key.
                // If it can, it is already correctly encrypted. If not, we treat it as plaintext and encrypt.
                let is_encrypted =
                    crate::runtime::crypto::decrypt_token(&token, master_key).is_ok();
                if !is_encrypted {
                    let encrypted = crate::runtime::crypto::encrypt_token(&token, master_key)?;
                    sqlx::query("UPDATE bot_credentials SET token = $1 WHERE id = $2")
                        .bind(encrypted)
                        .bind(id)
                        .execute(&mut **tx)
                        .await?;
                }
            }
        }

        tracing::info!(
            version = 8,
            "migration applied: vtoken hashed and bot_credentials encrypted"
        );
        Ok(())
    }

    // ─── Non-transactional single-version migrators (test surface only) ───
    //
    // These open their own transaction and delegate to the `_tx` variants,
    // so the SQL logic is never duplicated. Production code uses `run_migrations`.

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v1(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v1_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v2(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v2_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v3(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v3_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v4(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v4_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v5(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v5_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v6(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v6_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v7(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v7_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v8(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v8_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn migrate_to_v9_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 9).await? {
            return Ok(());
        }
        // Guard: the messages table may be absent on environments that applied
        // v1-v5 as a schema stub (e.g. partial-schema tests). Skip the index
        // DDL when the table is missing; in production the table always exists
        // after v5 has run for real.
        let table_exists = match self.kind {
            DatabaseKind::Sqlite => {
                sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages'")
                    .fetch_optional(&mut **tx)
                    .await?
                    .is_some()
            }
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.tables \
                 WHERE table_name = 'messages' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };

        if table_exists {
            sqlx::query(V9_MESSAGES_LOOKUP_IDX)
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("DDL failed: {V9_MESSAGES_LOOKUP_IDX}\n  Error: {e}")
                })?;
            tracing::info!(version = 9, "migration applied: messages lookup index");
        } else {
            tracing::debug!(
                "v9 migration: messages table absent (partial schema), skipping index creation"
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v9(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v9_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn migrate_to_v10_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 10).await? {
            return Ok(());
        }
        // Guard: clients table may be absent in partial-schema test environments.
        let table_exists = match self.kind {
            DatabaseKind::Sqlite => {
                sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name='clients'")
                    .fetch_optional(&mut **tx)
                    .await?
                    .is_some()
            }
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.tables \
                 WHERE table_name = 'clients' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };

        if table_exists {
            for ddl in V10_DDLS {
                sqlx::query(ddl)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| anyhow::anyhow!("DDL failed: {ddl}\n  Error: {e}"))?;
            }
            tracing::info!(version = 10, "migration applied: client persona columns");
        } else {
            tracing::debug!(
                "v10 migration: clients table absent (partial schema), skipping persona columns"
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v10(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v10_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// v11: Add `a2a_depth` column to `active_sessions` for A2A call-depth tracking.
    pub(super) async fn migrate_to_v11_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 11).await? {
            return Ok(());
        }
        // Guard: active_sessions table may be absent in partial-schema test environments.
        let table_exists = match self.kind {
            DatabaseKind::Sqlite => sqlx::query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_sessions'",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.tables \
                 WHERE table_name = 'active_sessions' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };

        if table_exists {
            let col_exists = match self.kind {
                DatabaseKind::Sqlite => sqlx::query(
                    "SELECT 1 FROM pragma_table_info('active_sessions') WHERE name = 'a2a_depth'",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
                DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                    "SELECT 1 FROM information_schema.columns \
                     WHERE table_name = 'active_sessions' AND column_name = 'a2a_depth' LIMIT 1",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
            };
            if !col_exists {
                sqlx::query(V11_ADD_A2A_DEPTH)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("DDL failed: {V11_ADD_A2A_DEPTH}\n  Error: {e}")
                    })?;
            }
            tracing::info!(version = 11, "migration applied: active_sessions.a2a_depth");
        } else {
            tracing::debug!(
                "v11 migration: active_sessions table absent (partial schema), skipping a2a_depth column"
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v11(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v11_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// v12: Add `description` column to `clients` for MCP `list_agents`.
    pub(super) async fn migrate_to_v12_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 12).await? {
            return Ok(());
        }
        // Guard: clients table may be absent in partial-schema test environments.
        let table_exists = match self.kind {
            DatabaseKind::Sqlite => {
                sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name='clients'")
                    .fetch_optional(&mut **tx)
                    .await?
                    .is_some()
            }
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = 'clients' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };

        if table_exists {
            let col_exists = match self.kind {
                DatabaseKind::Sqlite => sqlx::query(
                    "SELECT 1 FROM pragma_table_info('clients') WHERE name = 'description'",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
                DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                    "SELECT 1 FROM information_schema.columns \
                     WHERE table_name = 'clients' AND column_name = 'description' LIMIT 1",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
            };
            if !col_exists {
                sqlx::query(V12_ADD_DESCRIPTION)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("DDL failed: {V12_ADD_DESCRIPTION}\n  Error: {e}")
                    })?;
            }
            tracing::info!(version = 12, "migration applied: clients.description");
        } else {
            tracing::debug!(
                "v12 migration: clients table absent (partial schema), skipping description column"
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v12(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v12_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// v13: Add `ilink_msg_id` column + lookup index to `messages` for exact
    /// quote-reply routing via the iLink-preserved `ref_msg.message_item.msg_id`.
    pub(super) async fn migrate_to_v13_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 13).await? {
            return Ok(());
        }
        // Guard: messages table may be absent in partial-schema test environments.
        let table_exists = match self.kind {
            DatabaseKind::Sqlite => {
                sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages'")
                    .fetch_optional(&mut **tx)
                    .await?
                    .is_some()
            }
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = 'messages' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };

        if table_exists {
            let col_exists = match self.kind {
                DatabaseKind::Sqlite => sqlx::query(
                    "SELECT 1 FROM pragma_table_info('messages') WHERE name = 'ilink_msg_id'",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
                DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                    "SELECT 1 FROM information_schema.columns \
                     WHERE table_name = 'messages' AND column_name = 'ilink_msg_id' LIMIT 1",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
            };
            if !col_exists {
                sqlx::query(V13_ADD_ILINK_MSG_ID)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("DDL failed: {V13_ADD_ILINK_MSG_ID}\n  Error: {e}")
                    })?;
            }
            tracing::info!(version = 13, "migration applied: messages.ilink_msg_id");
        } else {
            tracing::debug!(
                "v13 migration: messages table absent (partial schema), skipping ilink_msg_id column"
            );
        }
        Ok(())
    }

    pub(super) async fn migrate_to_v14_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<()> {
        if !self.try_claim_migration_in_tx(tx, 14).await? {
            return Ok(());
        }
        let table_exists = match self.kind {
            DatabaseKind::Sqlite => sqlx::query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='backend_sessions_v2'",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
            DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = 'backend_sessions_v2' LIMIT 1",
            )
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
        };
        if table_exists {
            let col_exists = match self.kind {
                DatabaseKind::Sqlite => sqlx::query(
                    "SELECT 1 FROM pragma_table_info('backend_sessions_v2') WHERE name = 'last_usage_json'",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
                DatabaseKind::Postgres | DatabaseKind::MySql => sqlx::query(
                    "SELECT 1 FROM information_schema.columns                      WHERE table_name = 'backend_sessions_v2' AND column_name = 'last_usage_json' LIMIT 1",
                )
                .fetch_optional(&mut **tx)
                .await?
                .is_some(),
            };
            if !col_exists {
                sqlx::query(V14_ADD_SESSION_USAGE)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("DDL failed: {V14_ADD_SESSION_USAGE}\n  Error: {e}")
                    })?;
            }
            tracing::info!(
                version = 14,
                "migration applied: backend_sessions_v2.last_usage_json"
            );
        } else {
            tracing::debug!(
                "v14 migration: backend_sessions_v2 absent (partial schema), skipping last_usage_json"
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v13(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v13_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) async fn migrate_to_v14(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        self.migrate_to_v14_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    /// v5 `CREATE TABLE messages` DDL, with the `id` clause selected by driver.
    /// Pulled out of `migrate_to_v5` so the test surface can call all three
    /// branches directly without spinning up a Postgres or MySQL connection.
    ///
    /// Each form is portable to the named driver only. The only divergence
    /// between the three forms is the `id` clause and, for MySQL, the column
    /// type (`BIGINT` is required because MySQL's `AUTO_INCREMENT` on `INTEGER`
    /// is silently truncated to `INT(11)`, which then collides with the sqlx
    /// `i64` decoder used by `save_message`'s `last_insert_id` path).
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
    ///
    /// Allowed in non-test builds because the only callers live in
    /// `store_tests`; the `cfg_attr(not(test), allow(...))` keeps the
    /// production build warning-clean while still letting clippy flag a
    /// real dead-code regression in the test build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) async fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        if !is_safe_identifier(table) || !is_safe_identifier(column) {
            return Ok(false);
        }

        match self.kind {
            DatabaseKind::Sqlite => {
                let pragma_sql =
                    format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'");
                let row = sqlx::query(&pragma_sql)
                    .fetch_optional(&self.pool)
                    .await
                    .unwrap_or(None);
                Ok(row.is_some())
            }
            DatabaseKind::Postgres | DatabaseKind::MySql => {
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
    #[allow(dead_code)]
    pub(super) async fn ddl(&self, sql: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| anyhow::anyhow!("DDL failed: {sql}\n  Error: {e}"))?;
        Ok(())
    }
}

// ─── Migration SQL constants (embedded from migrations/ directory) ─────────────
//
// SQL lives in migrations/000N_*.sql for editor support (syntax highlighting,
// formatting). Driver-specific SQL that cannot be expressed in a single
// portable file remains inline above (v5 CREATE TABLE, v7 de-dup).

const V1_DDLS: &[&str] = &[include_str!("../../migrations/0001_initial_schema.sql")];

const V2_DDLS: &[&str] = &[include_str!("../../migrations/0002_backend_sessions.sql")];

const V3_CREATE_IDX: &str = include_str!("../../migrations/0003_context_token_real_ctx_index.sql");

const V4_ALTER_ADD_CREATED_AT: &str = "ALTER TABLE context_token_map ADD COLUMN created_at TEXT";

const V4_CREATE_IDX: &str = "CREATE INDEX IF NOT EXISTS idx_context_token_map_created_at \
     ON context_token_map (created_at DESC)";

const V5_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_messages_vctx_created \
     ON messages (vctx, created_at DESC)",
    "CREATE INDEX IF NOT EXISTS idx_messages_peer_role_created \
     ON messages (peer_user_id, role, created_at DESC)",
];

const V6_NORMALIZE_PEER_USER_ID: &str =
    include_str!("../../migrations/0006_normalize_peer_user_id.sql");

/// SQLite-specific: remove duplicate non-empty peer_user_id rows before creating
/// the unique index. Keeps the row with the lowest rowid per conv_key (oldest entry),
/// consistent with the PostgreSQL path which also keeps the oldest (MIN ctid).
const V7_DEDUP_PEER_USER_ID_SQLITE: &str = "DELETE FROM context_token_map \
     WHERE peer_user_id != '' \
       AND rowid NOT IN ( \
           SELECT MIN(rowid) FROM context_token_map \
           WHERE peer_user_id != '' \
           GROUP BY peer_user_id \
       )";

/// PostgreSQL-specific: same de-dup using ctid instead of rowid.
const V7_DEDUP_PEER_USER_ID_POSTGRES: &str = "DELETE FROM context_token_map \
     WHERE peer_user_id != '' \
       AND ctid NOT IN ( \
           SELECT MIN(ctid) FROM context_token_map \
           WHERE peer_user_id != '' \
           GROUP BY peer_user_id \
       )";

const V7_UNIQUE_IDX_PEER_USER_ID: &str =
    include_str!("../../migrations/0007_peer_user_id_unique_index.sql");

const V8_DDL: &str = include_str!("../../migrations/0008_vtoken_and_bot_token_hash.sql");

const V9_MESSAGES_LOOKUP_IDX: &str =
    include_str!("../../migrations/0009_messages_lookup_index.sql");

const V10_DDLS: &[&str] = &[include_str!("../../migrations/0010_client_persona.sql")];

const V11_ADD_A2A_DEPTH: &str = include_str!("../../migrations/0011_a2a_depth.sql");

const V12_ADD_DESCRIPTION: &str = include_str!("../../migrations/0012_client_description.sql");

const V13_ADD_ILINK_MSG_ID: &str = include_str!("../../migrations/0013_messages_ilink_msg_id.sql");

const V14_ADD_SESSION_USAGE: &str = include_str!("../../migrations/0014_backend_session_usage.sql");
