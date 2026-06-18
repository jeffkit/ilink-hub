//! Context token map persistence — maps virtual context tokens to real iLink context tokens.

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use super::Store;

impl Store {
    // ─── Context token map ────────────────────────────────────────────────────

    /// Find or create a stable virtual context token for a conversation.
    ///
    /// Keyed on conversation identity stored in the `peer_user_id` column:
    ///   - group message  → `"group:<group_id>"`
    ///   - DM             → `"peer:<peer_user_id>"`
    ///   - neither known  → empty string (generates a fresh vctx every call)
    ///
    /// If a row for this conv_key already exists, updates `real_ctx` to the latest
    /// value (WeChat issues a new real_ctx on every message) and returns the existing vctx.
    /// Otherwise inserts a fresh vctx. The result is stable across hub restarts because
    /// it is always read from DB — no in-memory state required for correctness.
    pub async fn find_or_create_vctx(
        &self,
        peer_user_id: &str,
        group_id: Option<&str>,
        real_ctx: &str,
    ) -> Result<String> {
        let conv_key: String = if let Some(g) = group_id.filter(|s| !s.is_empty()) {
            format!("group:{g}")
        } else if !peer_user_id.is_empty() {
            format!("peer:{peer_user_id}")
        } else {
            String::new()
        };

        if !conv_key.is_empty() {
            let existing = sqlx::query(
                "SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1",
            )
            .bind(&conv_key)
            .fetch_optional(&self.pool)
            .await?;

            if let Some(row) = existing {
                let vctx: String = row.get("vctx");
                sqlx::query("UPDATE context_token_map SET real_ctx = $1 WHERE vctx = $2")
                    .bind(real_ctx)
                    .bind(&vctx)
                    .execute(&self.pool)
                    .await?;
                return Ok(vctx);
            }
        }

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

        // Collision fallback (UUID collision is astronomically unlikely).
        let row = if !conv_key.is_empty() {
            sqlx::query(
                "SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1",
            )
            .bind(&conv_key)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query("SELECT vctx FROM context_token_map WHERE vctx = $1")
                .bind(&candidate)
                .fetch_optional(&self.pool)
                .await?
        };

        Ok(row
            .map(|r| r.get::<String, _>("vctx"))
            .unwrap_or(candidate))
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

#[cfg(test)]
mod tests {
    use super::Store;

    #[tokio::test]
    async fn find_or_create_vctx_same_peer_returns_same_vctx() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let v1 = store.find_or_create_vctx("user@wx", None, "ctx-1").await.unwrap();
        let v2 = store.find_or_create_vctx("user@wx", None, "ctx-2").await.unwrap();
        assert_eq!(v1, v2, "same peer must always get the same vctx");
    }

    #[tokio::test]
    async fn find_or_create_vctx_updates_real_ctx() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let vctx = store.find_or_create_vctx("user@wx", None, "ctx-first").await.unwrap();
        let vctx2 = store.find_or_create_vctx("user@wx", None, "ctx-second").await.unwrap();
        assert_eq!(vctx, vctx2);
        let resolved = store.resolve_context_token_full(&vctx).await.unwrap();
        assert_eq!(
            resolved,
            Some(("ctx-second".to_string(), "peer:user@wx".to_string())),
        );
    }

    #[tokio::test]
    async fn find_or_create_vctx_group_returns_stable_vctx() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let v1 = store.find_or_create_vctx("user1@wx", Some("group123"), "ctx-a").await.unwrap();
        let v2 = store.find_or_create_vctx("user2@wx", Some("group123"), "ctx-b").await.unwrap();
        assert_eq!(v1, v2, "same group_id must map to the same vctx");
    }

    #[tokio::test]
    async fn find_or_create_vctx_empty_ids_generate_new_vctx() {
        let store = Store::connect("sqlite::memory:").await.expect("connect");
        let v1 = store.find_or_create_vctx("", None, "ctx-x").await.unwrap();
        let v2 = store.find_or_create_vctx("", None, "ctx-y").await.unwrap();
        assert_ne!(v1, v2, "empty ids must not accidentally share a vctx");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn find_or_create_vctx_concurrent_same_peer_no_duplicate() {
        let store = std::sync::Arc::new(
            Store::connect("sqlite::memory:").await.expect("connect"),
        );
        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                s.find_or_create_vctx("concurrent-user", None, &format!("ctx-{i}"))
                    .await
                    .expect("find_or_create_vctx")
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.expect("task"));
        }
        let first = results[0].clone();
        for v in &results {
            assert_eq!(*v, first, "concurrent calls produced divergent vctx: {:?}", results);
        }
    }
}
