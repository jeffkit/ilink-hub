//! Backend session and active-session persistence.

use anyhow::Result;
use sqlx::Row;

use super::Store;

impl Store {
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
        .fetch_optional(&self.rpool)
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
        .fetch_all(&self.rpool)
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

        // SQLite's SQLITE_MAX_VARIABLE_NUMBER defaults to 999. Query 1 binds 2 params per pair
        // and query 2 binds 3 params per pair, so chunk at 200 pairs (600 params max) to stay
        // safely under both limits across all drivers.
        if pairs.len() > 200 {
            let mut merged = std::collections::HashMap::new();
            for chunk in pairs.chunks(200) {
                // Call the non-recursive inner query directly to avoid async recursion.
                let partial = self.get_hub_ext_batch_inner(chunk).await?;
                merged.extend(partial);
            }
            return Ok(merged);
        }

        self.get_hub_ext_batch_inner(pairs).await
    }

    async fn get_hub_ext_batch_inner(
        &self,
        pairs: &[(String, String)], // (vctx, vtoken), len must be <= 200
    ) -> Result<std::collections::HashMap<(String, String), (String, Option<String>)>> {
        debug_assert!(!pairs.is_empty());
        debug_assert!(pairs.len() <= 200);

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
        let active_rows = qb.build().fetch_all(&self.rpool).await?;

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
        let session_rows = qb2.build().fetch_all(&self.rpool).await?;

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

    /// Get active session name AND its backend_session_id in a single query.
    ///
    /// Replaces the two-step `get_active_session_name` + `get_backend_session` pattern used
    /// in `build_hub_ext_for_vctx`. A single query eliminates the TOCTOU window where
    /// `/session use` could switch sessions between the two calls, and halves the DB
    /// round-trips on the hot inbound-message path.
    ///
    /// Strategy: resolve the active session name first (defaulting to "default" when absent),
    /// then fetch the backend session ID for that name — all in one CTE / subquery so the
    /// two values come from a consistent snapshot.
    ///
    /// Returns `(session_name, Option<backend_session_id>)`.
    pub async fn get_hub_ext_single(
        &self,
        vctx: &str,
        vtoken: &str,
    ) -> Result<(String, Option<String>)> {
        // Use a CTE to resolve the session name once and reuse it in the JOIN condition.
        // COALESCE handles the common case where no active_sessions row exists yet
        // (first message in a conversation) and falls back to 'default'.
        let row = sqlx::query(
            "WITH resolved AS ( \
               SELECT COALESCE( \
                 (SELECT session_name FROM active_sessions \
                  WHERE vctx = $1 AND vtoken = $2 LIMIT 1), \
                 'default' \
               ) AS session_name \
             ) \
             SELECT r.session_name, b.backend_session_id \
             FROM resolved r \
             LEFT JOIN backend_sessions_v2 b \
               ON b.vctx = $1 AND b.vtoken = $2 AND b.session_name = r.session_name",
        )
        .bind(vctx)
        .bind(vtoken)
        .fetch_optional(&self.rpool)
        .await?;

        match row {
            Some(r) => {
                let name: String = r.get("session_name");
                let sid: Option<String> = r.try_get("backend_session_id").ok().flatten();
                let sid = sid.filter(|s| !s.trim().is_empty());
                Ok((name, sid))
            }
            // CTE always returns one row, so None here means a DB error — fall back to default.
            None => Ok(("default".to_string(), None)),
        }
    }

    /// Get the active session name for a (vctx, vtoken) pair (defaults to "default").
    pub async fn get_active_session_name(&self, vctx: &str, vtoken: &str) -> Result<String> {
        let row =
            sqlx::query("SELECT session_name FROM active_sessions WHERE vctx = $1 AND vtoken = $2")
                .bind(vctx)
                .bind(vtoken)
                .fetch_optional(&self.rpool)
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

    /// Upsert active session together with the A2A call depth for a (vctx, vtoken) pair.
    ///
    /// Called on every inbound message dispatch:
    /// - Regular user messages: `a2a_depth = 0`
    /// - Synthetic A2A messages: `a2a_depth = parent_depth + 1`
    ///
    /// This ensures `get_active_ctx_for_vtoken` can always find a row to check depth.
    pub async fn set_active_session_with_depth(
        &self,
        vctx: &str,
        vtoken: &str,
        session_name: &str,
        a2a_depth: u8,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO active_sessions (vctx, vtoken, session_name, a2a_depth)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (vctx, vtoken) DO UPDATE SET
                session_name = excluded.session_name,
                a2a_depth    = excluded.a2a_depth,
                updated_at   = CURRENT_TIMESTAMP
            "#,
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .bind(i32::from(a2a_depth))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve the most recently active conversation context for a given vtoken.
    ///
    /// Returns `(vctx, real_ctx, peer_user_id, a2a_depth)` by joining `active_sessions`
    /// with `context_token_map`, ordered by most recently updated session.
    ///
    /// Used by the MCP `call_agent` handler to auto-fill context without requiring the
    /// calling LLM to pass hidden `_hub_vctx` / `_hub_real_ctx` / `_hub_peer` arguments.
    pub async fn get_active_ctx_for_vtoken(
        &self,
        vtoken: &str,
    ) -> Result<Option<ActiveCtxInfo>> {
        let row = sqlx::query(
            "SELECT a.vctx, \
                    COALESCE(a.a2a_depth, 0) AS a2a_depth, \
                    c.real_ctx, \
                    COALESCE(c.peer_user_id, '') AS peer_user_id \
             FROM active_sessions a \
             JOIN context_token_map c ON a.vctx = c.vctx \
             WHERE a.vtoken = $1 \
             ORDER BY a.updated_at DESC \
             LIMIT 1",
        )
        .bind(vtoken)
        .fetch_optional(&self.rpool)
        .await?;

        Ok(row.map(|r| {
            let depth_raw: i32 = r.try_get("a2a_depth").unwrap_or(0);
            ActiveCtxInfo {
                vctx: r.get("vctx"),
                real_ctx: r.get("real_ctx"),
                peer_user_id: r.get("peer_user_id"),
                a2a_depth: u8::try_from(depth_raw.max(0)).unwrap_or(u8::MAX),
            }
        }))
    }
}

/// Active conversation context info returned by `get_active_ctx_for_vtoken`.
#[derive(Debug, Clone)]
pub struct ActiveCtxInfo {
    pub vctx: String,
    pub real_ctx: String,
    pub peer_user_id: String,
    /// Current A2A call-chain depth (0 = direct user message).
    pub a2a_depth: u8,
}

#[derive(Debug, Clone)]
pub struct BackendSessionRow {
    pub session_name: String,
    pub backend_session_id: String,
}
