//! Context-token map (virtual context resolution) persistence.

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use super::{DatabaseKind, Store};

/// Detect the "ON CONFLICT clause references a constraint that does not exist" error
/// using structured error codes, avoiding fragile string matching.
///
/// SQLite: error code 1 (SQLITE_ERROR) with message containing "conflict clause".
/// PostgreSQL: SqlState "42P10" (invalid_column_reference) or "42704" (undefined_object).
fn is_missing_constraint_error(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db_err) => {
            // PostgreSQL returns SqlState codes for constraint-name errors
            if let Some(code) = db_err.code() {
                // 42P10: invalid_column_reference (ON CONFLICT target doesn't match any constraint)
                // 42704: undefined_object (named constraint doesn't exist)
                if matches!(code.as_ref(), "42P10" | "42704") {
                    return true;
                }
            }
            // SQLite returns error code 1 (SQLITE_ERROR); discriminate by message fragment.
            // This is narrower than the old check: we only match the exact SQLite phrasing.
            let msg = db_err.message().to_lowercase();
            msg.contains("conflict clause does not match")
        }
        _ => false,
    }
}

impl Store {
    /// Find or create a stable virtual context token for a conversation.
    ///
    /// Uses `conv_key` (computed from `peer_user_id` / `group_id`) as the stable identifier
    /// stored in the `peer_user_id` column.
    ///
    /// On SQLite and PostgreSQL (v7+ schema): single-statement upsert via
    /// `INSERT ... ON CONFLICT (peer_user_id) DO UPDATE SET real_ctx = EXCLUDED.real_ctx
    /// RETURNING vctx` — fully race-free under concurrent callers.
    ///
    /// On MySQL (no partial index support) and when conv_key is empty: falls back to the
    /// original SELECT + INSERT two-step path, which is safe on the serialised single-
    /// connection write pool used for SQLite, and acceptable on MySQL where concurrent
    /// writers are rare in practice.
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

        // On SQLite/Postgres with a non-empty conv_key: attempt a single-statement race-free
        // upsert. Requires the v7 partial unique index on peer_user_id. If the index does not
        // exist yet (pre-v7 schema or mid-migration state), fall through to the two-step path.
        if !conv_key.is_empty()
            && matches!(self.kind, DatabaseKind::Sqlite | DatabaseKind::Postgres)
        {
            let candidate = format!("vctx_{}", Uuid::new_v4().simple());
            let result = sqlx::query(
                "INSERT INTO context_token_map (vctx, real_ctx, peer_user_id, created_at) \
                 VALUES ($1, $2, $3, CURRENT_TIMESTAMP) \
                 ON CONFLICT (peer_user_id) DO UPDATE \
                     SET real_ctx = EXCLUDED.real_ctx \
                 RETURNING vctx",
            )
            .bind(&candidate)
            .bind(real_ctx)
            .bind(&conv_key)
            .fetch_one(&self.pool)
            .await;

            match result {
                Ok(row) => return Ok(row.get("vctx")),
                Err(e) => {
                    // If the v7 index does not exist yet, the ON CONFLICT clause references a
                    // non-existent constraint and the DB returns a specific error. Detect this
                    // via structured error codes rather than string matching so that genuine
                    // constraint violations are never silently swallowed.
                    if !is_missing_constraint_error(&e) {
                        return Err(e.into());
                    }
                    tracing::debug!(
                        "v7 index absent, falling back to two-step find_or_create_vctx"
                    );
                }
            }
        }

        // Fallback path: MySQL (no partial index) or empty conv_key.
        // SELECT first, then INSERT if absent. Safe on MySQL's serialised write pool.
        if !conv_key.is_empty() {
            let existing =
                sqlx::query("SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1")
                    .bind(&conv_key)
                    .fetch_optional(&self.rpool)
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

        // No existing row — insert a new one (conv_key may be empty here).
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

        // Resolve back in case of vctx collision or concurrent insert race (MySQL only).
        let row = if !conv_key.is_empty() {
            sqlx::query("SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1")
                .bind(&conv_key)
                .fetch_optional(&self.rpool)
                .await?
        } else {
            sqlx::query("SELECT vctx FROM context_token_map WHERE vctx = $1")
                .bind(&candidate)
                .fetch_optional(&self.rpool)
                .await?
        };

        Ok(row.map(|r| r.get::<String, _>("vctx")).unwrap_or(candidate))
    }

    pub async fn resolve_context_token(&self, vctx: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT real_ctx FROM context_token_map WHERE vctx = $1")
            .bind(vctx)
            .fetch_optional(&self.rpool)
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
        .fetch_optional(&self.rpool)
        .await?;
        Ok(row.map(|r| (r.get("real_ctx"), r.get("peer_user_id"))))
    }

    /// Resolve everything `sendmessage` needs in a single DB round-trip:
    /// `(real_ctx, peer_user_id, session_name)`.
    ///
    /// Combines `resolve_context_token_full` and `get_active_session_name` to
    /// eliminate the two serial queries on the hot outbound path. Returns `None`
    /// Find the `vctx` for a conversation identified by its `peer_user_id` scope
    /// (`"peer:<id>"` or `"group:<id>"`).  Used by the persona-footer quote fallback.
    pub async fn find_vctx_for_scope(&self, scope: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT vctx FROM context_token_map WHERE peer_user_id = $1 LIMIT 1")
            .bind(scope)
            .fetch_optional(&self.rpool)
            .await?;
        Ok(row.map(|r| r.get("vctx")))
    }

    /// Return the `vtoken` of the backend that owns `session_name` inside `vctx`.
    /// Looks up `backend_sessions_v2`; returns `None` when no matching row exists.
    pub async fn find_vtoken_for_session(
        &self,
        vctx: &str,
        session_name: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT vtoken FROM backend_sessions_v2 \
             WHERE vctx = $1 AND session_name = $2 \
             ORDER BY rowid DESC LIMIT 1",
        )
        .bind(vctx)
        .bind(session_name)
        .fetch_optional(&self.rpool)
        .await?;
        Ok(row.map(|r| r.get("vtoken")))
    }

    /// when the vctx is unknown (caller should 400).
    pub async fn resolve_send_context(
        &self,
        vctx: &str,
        vtoken: &str,
    ) -> Result<Option<(String, String, String)>> {
        let row = sqlx::query(
            "SELECT c.real_ctx, \
                    COALESCE(c.peer_user_id, '') AS peer_user_id, \
                    COALESCE( \
                      (SELECT session_name FROM active_sessions \
                       WHERE vctx = $1 AND vtoken = $2 LIMIT 1), \
                      'default' \
                    ) AS session_name \
             FROM context_token_map c \
             WHERE c.vctx = $1",
        )
        .bind(vctx)
        .bind(vtoken)
        .fetch_optional(&self.rpool)
        .await?;
        Ok(row.map(|r| {
            (
                r.get("real_ctx"),
                r.get("peer_user_id"),
                r.get("session_name"),
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::is_missing_constraint_error;
    use sqlx::error::{DatabaseError, ErrorKind};
    use std::borrow::Cow;
    use std::error::Error;

    #[derive(Debug, Clone, Copy)]
    struct StubDbErr {
        code: Option<&'static str>,
        message: &'static str,
    }

    impl std::fmt::Display for StubDbErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl Error for StubDbErr {}

    impl DatabaseError for StubDbErr {
        fn message(&self) -> &str {
            self.message
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            self.code.map(Cow::Borrowed)
        }

        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }

        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }
    }

    fn db_err(code: Option<&'static str>, message: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(StubDbErr { code, message }))
    }

    #[test]
    fn is_missing_constraint_error_recognizes_postgres_codes() {
        assert!(is_missing_constraint_error(&db_err(
            Some("42P10"),
            "invalid_column_reference",
        )));
        assert!(is_missing_constraint_error(&db_err(
            Some("42704"),
            "undefined_object",
        )));
    }

    #[test]
    fn is_missing_constraint_error_recognizes_sqlite_conflict_clause_phrase() {
        assert!(is_missing_constraint_error(&db_err(
            None,
            "ON CONFLICT clause does not match any UNIQUE or PRIMARY KEY constraint",
        )));
    }

    #[test]
    fn is_missing_constraint_error_rejects_unrelated_database_errors() {
        assert!(!is_missing_constraint_error(&db_err(
            Some("2067"),
            "UNIQUE constraint failed: context_token_map.real_ctx",
        )));
    }

    #[test]
    fn is_missing_constraint_error_rejects_non_database_errors() {
        assert!(!is_missing_constraint_error(&sqlx::Error::RowNotFound));
    }
}
