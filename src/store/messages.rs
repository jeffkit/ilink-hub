//! Message history persistence — per-session conversation records.

use anyhow::Result;
use sqlx::Row;

use super::{sessions::SessionStatusEntry, Store};

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
    pub async fn get_session_status_per_vtoken(
        &self,
        vtokens: &[String],
    ) -> Result<std::collections::HashMap<String, SessionStatusEntry>> {
        if vtokens.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Step 1: find MAX(id) per vtoken across all roles — id is autoincrement so
        // MAX(id) always picks the row that was inserted last, even within the same second.
        let mut qb = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT vtoken, MAX(id) AS max_id FROM messages WHERE vtoken IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb.push(") GROUP BY vtoken");
        let max_rows = qb.build().fetch_all(&self.pool).await?;

        if max_rows.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Step 2: fetch the actual latest row by id to determine role.
        let mut qb2 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT id, vtoken, session_name, role FROM messages WHERE id IN (",
        );
        {
            let mut sep = qb2.separated(", ");
            for row in &max_rows {
                let max_id: i64 = row.get("max_id");
                sep.push_bind(max_id);
            }
        }
        qb2.push(")");
        let latest_rows = qb2.build().fetch_all(&self.pool).await?;

        // Step 3: find MAX(id) of user-role messages per vtoken (for the display snippet).
        // We always want to show the user's question, not the assistant's reply.
        // Also fetch created_at to compute elapsed processing time when waiting_for_reply.
        let mut qb3 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT vtoken, MAX(id) AS max_id FROM messages \
             WHERE role = 'user' AND vtoken IN (",
        );
        {
            let mut sep = qb3.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb3.push(") GROUP BY vtoken");
        let user_max_rows = qb3.build().fetch_all(&self.pool).await?;

        // (vtoken → (content, created_at))
        let mut user_content_map: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        if !user_max_rows.is_empty() {
            let mut qb4 = sqlx::QueryBuilder::<sqlx::Any>::new(
                "SELECT vtoken, content, created_at FROM messages WHERE id IN (",
            );
            {
                let mut sep = qb4.separated(", ");
                for row in &user_max_rows {
                    let max_id: i64 = row.get("max_id");
                    sep.push_bind(max_id);
                }
            }
            qb4.push(")");
            let user_rows = qb4.build().fetch_all(&self.pool).await?;
            for row in user_rows {
                let vtoken: String = row.get("vtoken");
                let content: String = row.get("content");
                let created_at: String = row.get("created_at");
                user_content_map
                    .entry(vtoken)
                    .or_insert((content, created_at));
            }
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

        // Step 1: for each (vtoken, session_name) find MAX(id) — determines the latest role.
        let mut qb = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT vtoken, session_name, MAX(id) AS max_id FROM messages WHERE vtoken IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb.push(") GROUP BY vtoken, session_name ORDER BY max_id DESC");
        let session_rows = qb.build().fetch_all(&self.pool).await?;

        if session_rows.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Collect (vtoken, session_name, max_id, role) by fetching the latest row per session.
        let max_ids: Vec<i64> = session_rows
            .iter()
            .map(|r| r.get::<i64, _>("max_id"))
            .collect();

        let mut qb2 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT id, vtoken, session_name, role FROM messages WHERE id IN (",
        );
        {
            let mut sep = qb2.separated(", ");
            for id in &max_ids {
                sep.push_bind(*id);
            }
        }
        qb2.push(")");
        let role_rows = qb2.build().fetch_all(&self.pool).await?;
        // (vtoken, session_name) → role of the latest message
        let mut role_map: std::collections::HashMap<(String, String), String> =
            std::collections::HashMap::new();
        for row in &role_rows {
            let vt: String = row.get("vtoken");
            let sn: String = row.get("session_name");
            let role: String = row.get("role");
            role_map.insert((vt, sn), role);
        }

        // Step 2: for each (vtoken, session_name), find the latest user message.
        let mut qb3 = sqlx::QueryBuilder::<sqlx::Any>::new(
            "SELECT vtoken, session_name, MAX(id) AS max_id FROM messages \
             WHERE role = 'user' AND vtoken IN (",
        );
        {
            let mut sep = qb3.separated(", ");
            for vt in vtokens {
                sep.push_bind(vt.as_str());
            }
        }
        qb3.push(") GROUP BY vtoken, session_name");
        let user_max_rows = qb3.build().fetch_all(&self.pool).await?;

        let user_max_ids: Vec<i64> = user_max_rows
            .iter()
            .map(|r| r.get::<i64, _>("max_id"))
            .collect();

        // (vtoken, session_name) → (content, created_at)
        let mut user_content_map: std::collections::HashMap<(String, String), (String, String)> =
            std::collections::HashMap::new();
        if !user_max_ids.is_empty() {
            let mut qb4 = sqlx::QueryBuilder::<sqlx::Any>::new(
                "SELECT vtoken, session_name, content, created_at FROM messages WHERE id IN (",
            );
            {
                let mut sep = qb4.separated(", ");
                for id in &user_max_ids {
                    sep.push_bind(*id);
                }
            }
            qb4.push(")");
            let user_rows = qb4.build().fetch_all(&self.pool).await?;
            for row in user_rows {
                let vt: String = row.get("vtoken");
                let sn: String = row.get("session_name");
                let content: String = row.get("content");
                let created_at: String = row.get("created_at");
                user_content_map.insert((vt, sn), (content, created_at));
            }
        }

        // Assemble result — preserve order from session_rows (most recent first).
        let mut map: std::collections::HashMap<String, Vec<SessionStatusEntry>> =
            std::collections::HashMap::new();
        for row in &session_rows {
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
