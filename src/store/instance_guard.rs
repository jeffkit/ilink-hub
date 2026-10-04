//! Single-instance guard keyed on the configured database (issue #28).
//!
//! Client registry, message queue, router and poll tracker all live in-process,
//! so two hubs sharing one `DATABASE_URL` would each own half the state and
//! fight over the same iLink bot session while silently splitting messages.
//! `run_serve` therefore takes a process-lifetime lock derived from the
//! database URL and fails fast when it is already held.
//!
//! The lock is released by the kernel (`flock` on the SQLite lock file) or by
//! the database (session-scoped Postgres advisory lock) as soon as the process
//! dies — SIGKILL and crashes included — so a leftover `<db>.lock` file never
//! blocks a restart.

use anyhow::Result;

use super::{sqlite_file_path, DatabaseKind, Store};

/// Fixed advisory-lock namespace id ("ilink_hu" as ASCII). The value only has
/// to be stable across versions and unlikely to collide with other apps.
#[cfg(feature = "postgres")]
const PG_ADVISORY_LOCK_KEY: i64 = 0x696C_696E_6B5F_6875;

/// Resources whose lifetime equals the guard's: dropping them releases the
/// kernel lock / database session lock.
#[allow(dead_code)] // payloads are held only for their `Drop` side effect
enum Held {
    /// `sqlite::memory:`, MySQL, and non-unix builds — nothing to lock.
    None,
    #[cfg(unix)]
    SqliteFile(std::fs::File),
    #[cfg(feature = "postgres")]
    Session(sqlx::AnyConnection),
}

pub(crate) struct InstanceGuard {
    /// Held purely for its `Drop` side effect; never read.
    #[allow(dead_code)]
    held: Held,
}

impl Store {
    /// Take the single-instance lock for `database_url`, failing fast if
    /// another hub already holds it.
    pub(crate) async fn acquire_instance_guard(&self, database_url: &str) -> Result<InstanceGuard> {
        let held = match self.kind {
            DatabaseKind::Sqlite => acquire_sqlite(database_url)?,
            DatabaseKind::Postgres => acquire_postgres(database_url).await?,
            // MySQL is not a supported runtime backend (runtime SQL uses the
            // `$N` placeholder form); no MySQL-specific lock is attempted.
            DatabaseKind::MySql => Held::None,
        };
        Ok(InstanceGuard { held })
    }
}

fn conflict_error() -> anyhow::Error {
    anyhow::anyhow!(
        "❌ Another instance of iLink Hub is already running on this database.\n\
         Hub runs as a single instance: a second hub sharing one DATABASE_URL would\n\
         silently split messages and fight over the same iLink bot session.\n\
         Stop the other instance, or give it its own DATABASE_URL, then retry.\n\
         See docs/deployment/docker.md (「使用 PostgreSQL 数据库」) and docs/guide/faq.md\n\
         (「多个 Hub 实例可以同时运行吗？」) — PostgreSQL raises concurrency for the ONE hub;\n\
         it does not make replicas safe."
    )
}

#[cfg(unix)]
fn acquire_sqlite(database_url: &str) -> Result<Held> {
    use std::os::fd::AsRawFd;

    let Some(db_path) = sqlite_file_path(database_url) else {
        return Ok(Held::None);
    };
    // The database file already exists (Store::connect created it); canonicalise
    // so two spellings of the same path share one lock file.
    let mut lock_path = std::fs::canonicalize(&db_path)?.into_os_string();
    lock_path.push(".lock");
    // `create(true)` + `truncate(false)` — never truncate: the target is the
    // lock file, not the database.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(std::path::PathBuf::from(lock_path))?;

    // flock is per open-file-description: the lock lives until this `File` is
    // closed (drop or process death), regardless of the lock file's existence.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err(conflict_error());
        }
        return Err(err.into());
    }
    Ok(Held::SqliteFile(file))
}

#[cfg(not(unix))]
fn acquire_sqlite(_database_url: &str) -> Result<Held> {
    // No kernel flock on this platform: the guard is a compile-time no-op.
    Ok(Held::None)
}

#[cfg(feature = "postgres")]
async fn acquire_postgres(database_url: &str) -> Result<Held> {
    use sqlx::Connection as _;

    // A dedicated connection, not a pool checkout: the pool would recycle the
    // session (max_lifetime) and silently drop the session-scoped advisory lock.
    let mut conn = sqlx::AnyConnection::connect(database_url).await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PG_ADVISORY_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if !acquired {
        return Err(conflict_error());
    }
    Ok(Held::Session(conn))
}

#[cfg(not(feature = "postgres"))]
async fn acquire_postgres(_database_url: &str) -> Result<Held> {
    // Unreachable: `DatabaseKind::from_url` bails out during `Store::connect`
    // when the `postgres` feature is off.
    Ok(Held::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_url(dir: &std::path::Path) -> String {
        format!("sqlite:{}", dir.join("ilink-hub.db").display())
    }

    #[tokio::test]
    async fn sqlite_guard_rejects_second_holder_on_same_file() {
        let tmp = tempfile::tempdir().unwrap();
        let url = file_url(tmp.path());

        let first = Store::connect(&url).await.unwrap();
        let guard = first.acquire_instance_guard(&url).await.unwrap();

        let second = Store::connect(&url).await.unwrap();
        let err = match second.acquire_instance_guard(&url).await {
            Ok(_) => panic!("second guard on the same database must be rejected"),
            Err(e) => e,
        };
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("another instance"), "unexpected error: {msg}");
        assert!(msg.contains("docker.md"), "unexpected error: {msg}");

        drop(guard);
    }

    #[tokio::test]
    async fn sqlite_guard_releases_when_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let url = file_url(tmp.path());

        let store = Store::connect(&url).await.unwrap();
        let guard = store.acquire_instance_guard(&url).await.unwrap();
        drop(guard);

        let guard = store.acquire_instance_guard(&url).await.unwrap();
        drop(guard);
    }

    #[tokio::test]
    async fn sqlite_guard_is_noop_for_memory_database() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let first = store
            .acquire_instance_guard("sqlite::memory:")
            .await
            .unwrap();
        let second = store
            .acquire_instance_guard("sqlite::memory:")
            .await
            .unwrap();
        drop((first, second));
    }
}
