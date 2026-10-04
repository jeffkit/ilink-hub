//! Driver-selected runtime SQL for the store layer.
//!
//! Runtime queries live here (DDL stays in `migrations.rs`) so the
//! driver-independent modules — `context.rs`, `messages.rs` — never carry a
//! dialect-specific token. A query using a SQLite-only token compiles fine and
//! even passes the SQLite test suite, but fails at runtime on PostgreSQL, where
//! the caller degrades silently. Static guards in `store_tests.rs` pin both
//! directions: the driver-independent modules stay free of SQLite-only tokens,
//! and each branch below uses only its own dialect's tokens.

use super::DatabaseKind;

/// `Store::find_vtoken_for_session` — which vtoken owns `session_name` in `vctx`.
///
/// The "newest row" ordering differs by driver, and the difference is
/// deliberate (see D3 in the issue #30 plan):
/// * SQLite — highest `rowid`. An upsert that hits the existing primary key
///   updates the row in place, so ordering by insertion age is stable; the
///   first writer of a given `(vctx, session_name)` keeps its position.
/// * PostgreSQL — highest `ctid`. An upsert writes a new physical tuple, so the
///   most recently *written* row sorts last; the last writer wins.
///
/// For the case this fixes (distinct vtokens inserting their own row for the
/// same `(vctx, session_name)`) both drivers agree: the newest insert wins.
pub(super) fn find_vtoken_for_session_sql(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => {
            "SELECT vtoken FROM backend_sessions_v2 \
             WHERE vctx = $1 AND session_name = $2 \
             ORDER BY rowid DESC LIMIT 1"
        }
        DatabaseKind::Postgres => {
            "SELECT vtoken FROM backend_sessions_v2 \
             WHERE vctx = $1 AND session_name = $2 \
             ORDER BY ctid DESC LIMIT 1"
        }
        // MySQL is a compile-time-only driver (docs/knowledge/api/configuration.md):
        // runtime SQL uses `$N` placeholders, which the MySQL protocol rejects. The
        // branch exists to keep the match exhaustive and uses MySQL's own timestamp
        // function instead of leaking a SQLite-only one, without claiming support.
        DatabaseKind::MySql => {
            "SELECT vtoken FROM backend_sessions_v2 \
             WHERE vctx = $1 AND session_name = $2 \
             ORDER BY created_at DESC LIMIT 1"
        }
    }
}

/// `Store::find_assistant_message_by_timestamp` — L1 timestamp quote-reply lookup.
///
/// SQLite stores `created_at` as `CURRENT_TIMESTAMP` text, hence the SQLite
/// branch's local date function; PostgreSQL receives a `TIMESTAMPTZ` text form
/// and MySQL a native `TIMESTAMP`, so both drive the comparison off
/// `EXTRACT(EPOCH ...)` / `UNIX_TIMESTAMP`. All three are wrapped in a cast to
/// `BIGINT` (or compared directly) so the bound `i64` parameters are inferred
/// as integers rather than `numeric`, which sqlx's `Any` pool cannot encode.
pub(super) fn find_assistant_message_by_timestamp_sql(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => {
            "SELECT vtoken, session_name FROM messages \
             WHERE peer_user_id = $1 AND role = 'assistant' \
               AND CAST(strftime('%s', created_at) AS INTEGER) BETWEEN $2 AND $3 \
             ORDER BY ABS(CAST(strftime('%s', created_at) AS INTEGER) - $4) ASC \
             LIMIT 1"
        }
        DatabaseKind::Postgres => {
            "SELECT vtoken, session_name FROM messages \
             WHERE peer_user_id = $1 AND role = 'assistant' \
               AND CAST(EXTRACT(EPOCH FROM CAST(created_at AS TIMESTAMPTZ)) AS BIGINT) \
                   BETWEEN $2 AND $3 \
             ORDER BY ABS(\
                 CAST(EXTRACT(EPOCH FROM CAST(created_at AS TIMESTAMPTZ)) AS BIGINT) - $4\
             ) ASC \
             LIMIT 1"
        }
        DatabaseKind::MySql => {
            "SELECT vtoken, session_name FROM messages \
             WHERE peer_user_id = $1 AND role = 'assistant' \
               AND UNIX_TIMESTAMP(created_at) BETWEEN $2 AND $3 \
             ORDER BY ABS(UNIX_TIMESTAMP(created_at) - $4) ASC \
             LIMIT 1"
        }
    }
}

/// Candidate rows for a `messages` retention sweep: `$1` is the cutoff (`Text`
/// on SQLite, epoch seconds elsewhere) and `$2` the batch limit.
///
/// SQLite has no date function here on purpose — comparing the fixed-width UTC
/// text `CURRENT_TIMESTAMP` writes (`%Y-%m-%d %H:%M:%S`) against such a string
/// orders exactly like time, so no SQLite-only function is needed and the same
/// predicate shape works everywhere.
pub(super) fn messages_expired_selection_sql(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => "SELECT id FROM messages WHERE created_at < $1 LIMIT $2",
        DatabaseKind::Postgres => {
            "SELECT id FROM messages \
             WHERE CAST(EXTRACT(EPOCH FROM CAST(created_at AS TIMESTAMPTZ)) AS BIGINT) < $1 \
             LIMIT $2"
        }
        DatabaseKind::MySql => {
            "SELECT id FROM messages WHERE UNIX_TIMESTAMP(created_at) < $1 LIMIT $2"
        }
    }
}

/// Batched `DELETE` for the rows [`messages_expired_selection_sql`] selects.
///
/// The delete repeats the selection as a subquery instead of using
/// `DELETE ... LIMIT`: SQLite only accepts `LIMIT` on `DELETE` when built with
/// `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`, which stock builds are not.
pub(super) fn messages_expired_delete_sql(kind: DatabaseKind) -> String {
    format!(
        "DELETE FROM messages WHERE id IN ({})",
        messages_expired_selection_sql(kind)
    )
}

/// Candidate rows for an `active_sessions` retention sweep (same binding contract
/// as the `messages` selection).
pub(super) fn active_sessions_expired_selection_sql(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Sqlite => {
            "SELECT vctx, vtoken FROM active_sessions WHERE updated_at < $1 LIMIT $2"
        }
        DatabaseKind::Postgres => {
            "SELECT vctx, vtoken FROM active_sessions \
             WHERE CAST(EXTRACT(EPOCH FROM CAST(updated_at AS TIMESTAMPTZ)) AS BIGINT) < $1 \
             LIMIT $2"
        }
        DatabaseKind::MySql => {
            "SELECT vctx, vtoken FROM active_sessions \
             WHERE UNIX_TIMESTAMP(updated_at) < $1 LIMIT $2"
        }
    }
}

/// Batched `DELETE` for the rows [`active_sessions_expired_selection_sql`] selects.
/// A row-value `IN` targets the `(vctx, vtoken)` primary key (supported by
/// SQLite 3.15+ and PostgreSQL).
pub(super) fn active_sessions_expired_delete_sql(kind: DatabaseKind) -> String {
    format!(
        "DELETE FROM active_sessions WHERE (vctx, vtoken) IN ({})",
        active_sessions_expired_selection_sql(kind)
    )
}

/// Value bound into a retention predicate.
///
/// SQLite compares the `CURRENT_TIMESTAMP` text column lexicographically against
/// UTC text; the other drivers compare an epoch-seconds expression against an
/// integer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RetentionCutoff {
    Text(String),
    Epoch(i64),
}

/// Build the cutoff for `cutoff_epoch_secs` in the form `kind`'s predicate expects.
pub(super) fn retention_cutoff(kind: DatabaseKind, cutoff_epoch_secs: i64) -> RetentionCutoff {
    match kind {
        DatabaseKind::Sqlite => RetentionCutoff::Text(
            chrono::DateTime::<chrono::Utc>::from_timestamp(cutoff_epoch_secs, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default(),
        ),
        _ => RetentionCutoff::Epoch(cutoff_epoch_secs),
    }
}
