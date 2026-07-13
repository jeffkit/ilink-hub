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
        self.save_message_with_msg_id(
            vctx,
            vtoken,
            session_name,
            peer_user_id,
            role,
            content,
            None,
        )
        .await
    }

    /// Same as [`save_message`](Self::save_message) but also persists the
    /// iLink `message_id` the Hub assigned to an outbound assistant reply.
    /// iLink preserves this id and echoes it back as
    /// `ref_msg.message_item.msg_id` on quote-reply, enabling exact routing
    /// (L0) instead of the ±10s timestamp fallback. `None` for user-side rows
    /// and pre-feature rows.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_message_with_msg_id(
        &self,
        vctx: &str,
        vtoken: Option<&str>,
        session_name: &str,
        peer_user_id: &str,
        role: &str,
        content: &str,
        ilink_msg_id: Option<i64>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO messages \
             (vctx, vtoken, session_name, peer_user_id, role, content, ilink_msg_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(vctx)
        .bind(vtoken)
        .bind(session_name)
        .bind(peer_user_id)
        .bind(role)
        .bind(content)
        .bind(ilink_msg_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find the most recent assistant message in a conversation whose content starts with
    /// `content_prefix`. Used as the L2 (content-prefix) DB fallback for quote-reply routing.
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

    /// Find the assistant message whose stored `ilink_msg_id` equals the given
    /// iLink message_id. This is the **L0 exact-match** resolver for quote-reply
    /// routing: iLink preserves the Hub-assigned `message_id` and echoes it back
    /// as `ref_msg.message_item.msg_id`, so this lookup uniquely identifies the
    /// quoted message — no timestamp window, no content prefix.
    ///
    /// `peer_user_id` is included as a defence-in-depth scope filter even though
    /// `ilink_msg_id` is globally unique (snowflake). Returns `(vtoken, session_name)`
    /// or `None` when no row carries that id (e.g. pre-feature rows, or a user-side
    /// message that was quoted — caller falls back to L1).
    pub async fn find_assistant_message_by_msg_id(
        &self,
        peer_user_id: &str,
        ilink_msg_id: i64,
    ) -> Result<Option<(String, Option<String>)>> {
        let row = sqlx::query(
            "SELECT vtoken, session_name FROM messages \
             WHERE peer_user_id = $1 AND role = 'assistant' AND ilink_msg_id = $2 \
             LIMIT 1",
        )
        .bind(peer_user_id)
        .bind(ilink_msg_id)
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

#[cfg(test)]
mod tests {
    use crate::store::Store;

    async fn make_store() -> Store {
        Store::connect("sqlite::memory:").await.expect("connect")
    }

    #[tokio::test]
    async fn save_message_and_list_messages_returns_saved_row() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "default",
                "user-1",
                "user",
                "hello",
            )
            .await
            .expect("save");
        let rows = store.list_messages("vctx-1", 10).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "hello");
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[0].vtoken, Some("vtok-A".to_string()));
    }

    #[tokio::test]
    async fn list_messages_filters_by_vctx_and_respects_limit() {
        let store = make_store().await;
        store
            .save_message("vctx-1", None, "default", "u1", "user", "in-vctx-1-a")
            .await
            .expect("save a");
        store
            .save_message("vctx-1", None, "default", "u1", "assistant", "in-vctx-1-b")
            .await
            .expect("save b");
        store
            .save_message("vctx-2", None, "default", "u1", "user", "in-vctx-2")
            .await
            .expect("save other vctx");
        // limit=1 must only return the one latest row for vctx-1
        let rows = store
            .list_messages("vctx-1", 1)
            .await
            .expect("list with limit");
        assert_eq!(rows.len(), 1, "limit=1 must return exactly 1 row");
        // Without limit restriction, both vctx-1 rows but not vctx-2 row
        let all = store.list_messages("vctx-1", 10).await.expect("list all");
        assert_eq!(all.len(), 2, "must return only rows for vctx-1");
        let contents: Vec<&str> = all.iter().map(|r| r.content.as_str()).collect();
        assert!(contents.contains(&"in-vctx-1-a"));
        assert!(contents.contains(&"in-vctx-1-b"));
        assert!(
            !contents.contains(&"in-vctx-2"),
            "vctx-2 row must not appear"
        );
    }

    #[tokio::test]
    async fn find_assistant_message_by_content_finds_matching_row() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "Hello World",
            )
            .await
            .expect("save");
        let result = store
            .find_assistant_message_by_content("user-1", "Hello")
            .await
            .expect("find");
        assert!(result.is_some());
        let (vtoken, session) = result.unwrap();
        assert_eq!(vtoken, "vtok-A");
        assert_eq!(session, Some("sess-1".to_string()));
    }

    #[tokio::test]
    async fn find_assistant_message_by_content_returns_none_when_no_match() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "Hello World",
            )
            .await
            .expect("save");
        let result = store
            .find_assistant_message_by_content("user-1", "Goodbye")
            .await
            .expect("find");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_assistant_message_by_content_does_not_match_user_role() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "user",
                "Hello from user",
            )
            .await
            .expect("save");
        let result = store
            .find_assistant_message_by_content("user-1", "Hello from user")
            .await
            .expect("find");
        assert!(result.is_none(), "must not match user-role messages");
    }

    #[tokio::test]
    async fn find_assistant_message_by_content_escapes_percent_wildcard() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "50%_off sale",
            )
            .await
            .expect("save target");
        store
            .save_message(
                "vctx-1",
                Some("vtok-B"),
                "sess-2",
                "user-1",
                "assistant",
                "50 anything sale",
            )
            .await
            .expect("save decoy");
        // Prefix "50%_off" must be escaped so % and _ are treated as literals
        let result = store
            .find_assistant_message_by_content("user-1", "50%_off")
            .await
            .expect("find");
        let (vtoken, _) = result.expect("must match the exact row");
        assert_eq!(
            vtoken, "vtok-A",
            "escaped prefix must match only the literal row"
        );
    }

    #[tokio::test]
    async fn find_assistant_message_by_timestamp_returns_match_within_window() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "Timed msg",
            )
            .await
            .expect("save");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let result = store
            .find_assistant_message_by_timestamp("user-1", now, 10)
            .await
            .expect("find");
        let (vtoken, session) = result.expect("must find message within ±10s window");
        assert_eq!(vtoken, "vtok-A");
        assert_eq!(session, Some("sess-1".to_string()));
    }

    #[tokio::test]
    async fn find_assistant_message_by_timestamp_returns_none_outside_window() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "Old msg",
            )
            .await
            .expect("save");
        // Target timestamp 1 hour in the past with a 5s window — must not match
        let past_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 3600;
        let result = store
            .find_assistant_message_by_timestamp("user-1", past_ts, 5)
            .await
            .expect("find");
        assert!(result.is_none(), "timestamp outside window must not match");
    }

    #[tokio::test]
    async fn find_assistant_message_by_msg_id_returns_exact_row() {
        let store = make_store().await;
        let msg_id = 1_783_912_191_000_000_111i64;
        store
            .save_message_with_msg_id(
                "vctx-1",
                Some("vtok-A"),
                "sess-A",
                "peer:user-1",
                "assistant",
                "exact msg",
                Some(msg_id),
            )
            .await
            .expect("save");
        let (vtoken, session) = store
            .find_assistant_message_by_msg_id("peer:user-1", msg_id)
            .await
            .expect("find")
            .expect("must find row by ilink_msg_id");
        assert_eq!(vtoken, "vtok-A");
        assert_eq!(session, Some("sess-A".to_string()));
    }

    #[tokio::test]
    async fn find_assistant_message_by_msg_id_ignores_user_role_rows() {
        let store = make_store().await;
        let msg_id = 1_783_912_191_000_000_222i64;
        // A user-side row carrying the same ilink_msg_id must not match.
        store
            .save_message_with_msg_id(
                "vctx-1",
                Some("vtok-A"),
                "sess-A",
                "peer:user-1",
                "user",
                "user msg",
                Some(msg_id),
            )
            .await
            .expect("save");
        let result = store
            .find_assistant_message_by_msg_id("peer:user-1", msg_id)
            .await
            .expect("find");
        assert!(result.is_none(), "user-role rows must be skipped");
    }

    #[tokio::test]
    async fn find_assistant_message_by_msg_id_scoped_by_peer() {
        let store = make_store().await;
        let msg_id = 1_783_912_191_000_000_333i64;
        store
            .save_message_with_msg_id(
                "vctx-1",
                Some("vtok-A"),
                "sess-A",
                "peer:other",
                "assistant",
                "other peer",
                Some(msg_id),
            )
            .await
            .expect("save");
        let result = store
            .find_assistant_message_by_msg_id("peer:user-1", msg_id)
            .await
            .expect("find");
        assert!(
            result.is_none(),
            "msg_id lookup must be scoped by peer_user_id"
        );
    }

    #[tokio::test]
    async fn list_messages_for_session_clamps_limit_min_to_one() {
        let store = make_store().await;
        store
            .save_message("vctx-1", Some("vtok-A"), "sess-1", "user-1", "user", "msg1")
            .await
            .expect("save");
        // limit=0 must be clamped to 1
        let rows = store
            .list_messages_for_session("vtok-A", "sess-1", 0)
            .await
            .expect("list");
        assert_eq!(rows.len(), 1, "limit=0 clamped to 1 must return 1 row");
    }

    #[tokio::test]
    async fn list_messages_for_session_clamps_limit_max_to_500() {
        let store = make_store().await;
        for i in 0..5 {
            store
                .save_message(
                    "vctx-1",
                    Some("vtok-A"),
                    "sess-1",
                    "user-1",
                    "user",
                    &format!("msg{i}"),
                )
                .await
                .expect("save");
        }
        // limit=600 clamped to 500; only 5 messages exist, so all 5 returned
        let rows = store
            .list_messages_for_session("vtok-A", "sess-1", 600)
            .await
            .expect("list");
        assert_eq!(
            rows.len(),
            5,
            "limit=600 clamped to 500 must still return all 5 rows"
        );
    }

    #[tokio::test]
    async fn get_session_status_per_vtoken_empty_input_returns_empty_map() {
        let store = make_store().await;
        let result = store.get_session_status_per_vtoken(&[]).await.expect("get");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_session_status_per_vtoken_waiting_true_when_last_role_is_user() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "AI reply",
            )
            .await
            .expect("save assistant");
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "user",
                "User followup",
            )
            .await
            .expect("save user");
        let result = store
            .get_session_status_per_vtoken(&["vtok-A".to_string()])
            .await
            .expect("get");
        let entry = result.get("vtok-A").expect("must have vtok-A");
        assert!(
            entry.waiting_for_reply,
            "last role=user → waiting_for_reply must be true"
        );
    }

    #[tokio::test]
    async fn get_session_status_per_vtoken_waiting_false_when_last_role_is_assistant() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "user",
                "User question",
            )
            .await
            .expect("save user");
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "AI reply",
            )
            .await
            .expect("save assistant");
        let result = store
            .get_session_status_per_vtoken(&["vtok-A".to_string()])
            .await
            .expect("get");
        let entry = result.get("vtok-A").expect("must have vtok-A");
        assert!(
            !entry.waiting_for_reply,
            "last role=assistant → waiting_for_reply must be false"
        );
    }

    #[tokio::test]
    async fn get_all_session_entries_per_vtoken_empty_input_returns_empty_map() {
        let store = make_store().await;
        let result = store
            .get_all_session_entries_per_vtoken(&[])
            .await
            .expect("get");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_all_session_entries_per_vtoken_groups_by_session() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "user",
                "Q in sess1",
            )
            .await
            .expect("save sess1");
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-2",
                "user-1",
                "assistant",
                "A in sess2",
            )
            .await
            .expect("save sess2");
        let result = store
            .get_all_session_entries_per_vtoken(&["vtok-A".to_string()])
            .await
            .expect("get");
        let entries = result.get("vtok-A").expect("must have vtok-A");
        assert_eq!(entries.len(), 2, "two sessions must produce two entries");
        let names: Vec<&str> = entries.iter().map(|e| e.session_name.as_str()).collect();
        assert!(names.contains(&"sess-1"));
        assert!(names.contains(&"sess-2"));
    }

    #[tokio::test]
    async fn get_all_session_entries_per_vtoken_waiting_true_when_last_role_is_user() {
        let store = make_store().await;
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "assistant",
                "AI reply",
            )
            .await
            .expect("save assistant");
        store
            .save_message(
                "vctx-1",
                Some("vtok-A"),
                "sess-1",
                "user-1",
                "user",
                "User followup",
            )
            .await
            .expect("save user");
        let result = store
            .get_all_session_entries_per_vtoken(&["vtok-A".to_string()])
            .await
            .expect("get");
        let entries = result.get("vtok-A").expect("vtok-A");
        let sess1 = entries
            .iter()
            .find(|e| e.session_name == "sess-1")
            .expect("sess-1");
        assert!(
            sess1.waiting_for_reply,
            "last role=user → waiting_for_reply must be true"
        );
    }
}
