//! Chat message history persistence.

use anyhow::Result;
use sqlx::Row;

use super::Store;

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

#[derive(Debug, Clone)]
pub struct SessionStatusEntry {
    pub session_name: String,
    pub last_user_content: Option<String>,
    /// `true` when the last stored message is from the user (AI has not replied yet).
    pub waiting_for_reply: bool,
    /// ISO-8601 timestamp of the last user message — used to compute elapsed time
    /// when `waiting_for_reply` is true. `None` when there are no user messages.
    pub user_msg_created_at: Option<String>,
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
        .fetch_optional(&self.rpool)
        .await?;
        Ok(row.map(|r| {
            let vtoken: Option<String> = r.get("vtoken");
            let session_name: String = r.get("session_name");
            (vtoken.unwrap_or_default(), Some(session_name))
        }))
    }

    /// Find the most recent assistant message in a conversation sent within ±`window_secs`
    /// of the given Unix timestamp. Used as a reliable fallback for quote-reply routing
    /// when the iLink protocol does not carry text in `ref_msg.message_item` (only a
    /// `create_time_ms` timestamp is available).
    ///
    /// Returns `(vtoken, session_name)` of the closest matching row, or `None`.
    pub async fn find_assistant_message_by_timestamp(
        &self,
        peer_user_id: &str,
        ref_unix_secs: i64,
        window_secs: i64,
    ) -> Result<Option<(String, Option<String>)>> {
        let lo = ref_unix_secs - window_secs;
        let hi = ref_unix_secs + window_secs;
        // SQLite stores created_at as "YYYY-MM-DD HH:MM:SS" (UTC). Cast via unixepoch().
        let row = sqlx::query(
            "SELECT vtoken, session_name FROM messages \
             WHERE peer_user_id = $1 AND role = 'assistant' \
               AND CAST(strftime('%s', created_at) AS INTEGER) BETWEEN $2 AND $3 \
             ORDER BY ABS(CAST(strftime('%s', created_at) AS INTEGER) - $4) ASC \
             LIMIT 1",
        )
        .bind(peer_user_id)
        .bind(lo)
        .bind(hi)
        .bind(ref_unix_secs)
        .fetch_optional(&self.rpool)
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
        .fetch_all(&self.rpool)
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

    /// Load messages for a specific (vtoken, session_name) pair, newest first.
    ///
    /// `limit` is clamped to `[1, 500]`. Returns rows ordered by id DESC (most recent first);
    /// callers that want chronological order should reverse the result.
    pub async fn list_messages_for_session(
        &self,
        vtoken: &str,
        session_name: &str,
        limit: i64,
    ) -> Result<Vec<MessageRow>> {
        let clamped = limit.clamp(1, 500);
        let rows = sqlx::query(
            "SELECT id, vctx, vtoken, session_name, peer_user_id, role, content, created_at \
             FROM messages WHERE vtoken = $1 AND session_name = $2 ORDER BY id DESC LIMIT $3",
        )
        .bind(vtoken)
        .bind(session_name)
        .bind(clamped)
        .fetch_all(&self.rpool)
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

    /// For each vtoken in the list, return a summary of the most recent conversation turn:
    /// `(session_name, last_user_content, waiting_for_reply)`.
    ///
    /// `waiting_for_reply` is `true` when the last stored message for that vtoken has
    /// `role = 'user'` — meaning the user sent a message but the assistant has not yet
    /// written a reply back to the messages table.
    ///
    /// Uses `MAX(id)` (autoincrement, strictly monotone) as the "latest row" selector,
    /// which is stable even when two rows share the same `created_at` second — portable
    /// across SQLite, PostgreSQL, and MySQL.
    #[allow(dead_code)]
    pub async fn get_session_status_per_vtoken(
        &self,
        vtokens: &[String],
    ) -> Result<std::collections::HashMap<String, SessionStatusEntry>> {
        if vtokens.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Round-trip 1 (was 2): join messages against a per-vtoken MAX(id) subquery to
        // retrieve the latest row's role and session_name in a single SQL statement.
        let mut qb = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT m.vtoken, m.session_name, m.role \
             FROM messages m \
             INNER JOIN (SELECT vtoken, MAX(id) AS max_id FROM messages \
                         WHERE vtoken IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb.push(") GROUP BY vtoken) t ON m.id = t.max_id");
        let latest_rows = qb.build().fetch_all(&self.rpool).await?;

        if latest_rows.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Round-trip 2 (was 2): same approach for the latest user-role message per vtoken,
        // fetching content and created_at for the status snippet and elapsed-time display.
        let mut qb2 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT m.vtoken, m.content, m.created_at \
             FROM messages m \
             INNER JOIN (SELECT vtoken, MAX(id) AS max_id FROM messages \
                         WHERE role = 'user' AND vtoken IN (",
        );
        {
            let mut sep = qb2.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb2.push(") GROUP BY vtoken) t ON m.id = t.max_id");
        let user_rows = qb2.build().fetch_all(&self.rpool).await?;

        // (vtoken → (content, created_at))
        let mut user_content_map: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        for row in user_rows {
            let vtoken: String = row.get("vtoken");
            let content: String = row.get("content");
            let created_at: String = row.get("created_at");
            user_content_map
                .entry(vtoken)
                .or_insert((content, created_at));
        }

        let mut map = std::collections::HashMap::new();
        for row in latest_rows {
            let vtoken: String = row.get("vtoken");
            let session: String = row.get("session_name");
            let role: String = row.get("role");
            let waiting = role == "user";
            let (last_user_msg, user_msg_created_at) = user_content_map
                .get(&vtoken)
                .map(|(c, t)| (Some(c.clone()), Some(t.clone())))
                .unwrap_or((None, None));
            map.entry(vtoken).or_insert(SessionStatusEntry {
                session_name: session,
                last_user_content: last_user_msg,
                waiting_for_reply: waiting,
                user_msg_created_at,
            });
        }
        Ok(map)
    }

    /// Return all active sessions (grouped by session_name) for the given vtokens.
    ///
    /// For each (vtoken, session_name) pair that has at least one message, returns a
    /// `SessionStatusEntry` with the latest user message and whether the AI has replied.
    /// Results are ordered by the timestamp of the most recent message in that session
    /// (most recent first within each vtoken).
    pub async fn get_all_session_entries_per_vtoken(
        &self,
        vtokens: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<SessionStatusEntry>>> {
        if vtokens.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Round-trip 1 (was 2): per (vtoken, session_name) MAX(id) subquery joined back to
        // messages to retrieve the latest row's role in one statement.
        // ORDER BY t.max_id DESC preserves "most recent session first" ordering.
        let mut qb = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT m.vtoken, m.session_name, m.role, t.max_id \
             FROM messages m \
             INNER JOIN (SELECT vtoken, session_name, MAX(id) AS max_id FROM messages \
                         WHERE vtoken IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb.push(") GROUP BY vtoken, session_name) t ON m.id = t.max_id ORDER BY t.max_id DESC");
        let latest_rows = qb.build().fetch_all(&self.rpool).await?;

        if latest_rows.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Build role map from the single result set.
        // (vtoken, session_name) → role of the latest message
        let mut role_map: std::collections::HashMap<(String, String), String> =
            std::collections::HashMap::new();
        for row in &latest_rows {
            let vt: String = row.get("vtoken");
            let sn: String = row.get("session_name");
            let role: String = row.get("role");
            role_map.insert((vt, sn), role);
        }

        // Round-trip 2 (was 2): same approach for the latest user-role message per
        // (vtoken, session_name), fetching content and created_at together.
        let mut qb2 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT m.vtoken, m.session_name, m.content, m.created_at \
             FROM messages m \
             INNER JOIN (SELECT vtoken, session_name, MAX(id) AS max_id FROM messages \
                         WHERE role = 'user' AND vtoken IN (",
        );
        {
            let mut sep = qb2.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb2.push(") GROUP BY vtoken, session_name) t ON m.id = t.max_id");
        let user_rows = qb2.build().fetch_all(&self.rpool).await?;

        // (vtoken, session_name) → (content, created_at)
        let mut user_content_map: std::collections::HashMap<(String, String), (String, String)> =
            std::collections::HashMap::new();
        for row in user_rows {
            let vt: String = row.get("vtoken");
            let sn: String = row.get("session_name");
            let content: String = row.get("content");
            let created_at: String = row.get("created_at");
            user_content_map.insert((vt, sn), (content, created_at));
        }

        // Assemble result — preserve order from latest_rows (most recent first).
        let mut map: std::collections::HashMap<String, Vec<SessionStatusEntry>> =
            std::collections::HashMap::new();
        for row in &latest_rows {
            let vt: String = row.get("vtoken");
            let sn: String = row.get("session_name");
            let role = role_map
                .get(&(vt.clone(), sn.clone()))
                .map(String::as_str)
                .unwrap_or("assistant");
            let waiting = role == "user";
            let (last_user_content, user_msg_created_at) = user_content_map
                .get(&(vt.clone(), sn.clone()))
                .map(|(c, t)| (Some(c.clone()), Some(t.clone())))
                .unwrap_or((None, None));
            map.entry(vt).or_default().push(SessionStatusEntry {
                session_name: sn,
                last_user_content,
                waiting_for_reply: waiting,
                user_msg_created_at,
            });
        }
        Ok(map)
    }
}
