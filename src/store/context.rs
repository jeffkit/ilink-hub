//! Context token map persistence — maps virtual context tokens to real iLink context tokens.

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use super::Store;

impl Store {
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
}
