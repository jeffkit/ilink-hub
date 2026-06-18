//! Context-token map (virtual context resolution) persistence.

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

    /// Find or create a stable virtual context token for a conversation.
    ///
    /// Uses `conv_key` (computed from `peer_user_id` / `group_id`) as the stable identifier
    /// stored in the `peer_user_id` column.  This avoids a schema change while letting group
    /// conversations reuse the same vctx across messages.
    ///
    /// Lookup order:
    /// 1. SELECT by conv_key → if found, UPDATE real_ctx and return existing vctx.
    /// 2. Otherwise INSERT a fresh vctx (with `ON CONFLICT (vctx) DO NOTHING` for safety).
    ///
    /// Returns the stable `vctx` for the conversation.
    pub async fn find_or_create_vctx(
        &self,
        peer_user_id: &str,
        group_id: Option<&str>,
        real_ctx: &str,
    ) -> Result<String> {
        // Compute the conversation key to use as the `peer_user_id` column value.
        // group:<id> for group messages, peer:<id> for DMs, or "" when neither is known.
        let conv_key: String = if let Some(g) = group_id.filter(|s| !s.is_empty()) {
            format!("group:{g}")
        } else if !peer_user_id.is_empty() {
            format!("peer:{peer_user_id}")
        } else {
            String::new()
        };

        // If we have a conv_key, try to find an existing row first.
        if !conv_key.is_empty() {
            let existing =
                sqlx::query("SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1")
                    .bind(&conv_key)
                    .fetch_optional(&self.pool)
                    .await?;

            if let Some(row) = existing {
                let vctx: String = row.get("vctx");
                // Update real_ctx to the latest value so sendmessage resolves correctly.
                sqlx::query("UPDATE context_token_map SET real_ctx = $1 WHERE vctx = $2")
                    .bind(real_ctx)
                    .bind(&vctx)
                    .execute(&self.pool)
                    .await?;
                return Ok(vctx);
            }
        }

        // No existing row — insert a new one.
        let candidate = format!("vctx_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO context_token_map (vctx, real_ctx, peer_user_id, created_at) \
             VALUES ($1, $2, $3, CURRENT_TIMESTAMP) \
             ON CONFLICT (vctx) DO NOTHING",
        )
        .bind(&candidate)
        .bind(real_ctx)
        .bind(&conv_key)
        .execute(&self.pool)
        .await?;

        // In the (extremely unlikely) vctx collision case, fall back to SELECT.
        let row = if !conv_key.is_empty() {
            sqlx::query("SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1")
                .bind(&conv_key)
                .fetch_optional(&self.pool)
                .await?
        } else {
            sqlx::query("SELECT vctx FROM context_token_map WHERE vctx = $1")
                .bind(&candidate)
                .fetch_optional(&self.pool)
                .await?
        };

        Ok(row.map(|r| r.get::<String, _>("vctx")).unwrap_or(candidate))
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
}
