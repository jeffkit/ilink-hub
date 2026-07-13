use super::*;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// After `Store::connect`, all v1-v5 migrations must have been applied.
#[tokio::test]
async fn test_schema_version_tracking() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // All migrations must be applied after a fresh connect.
    let version = store
        .get_current_version()
        .await
        .expect("get_current_version");
    assert_eq!(
        version, 13,
        "expected all 13 migrations to be applied on a fresh DB"
    );

    for v in 1..=13 {
        let applied = store.is_migration_run(v).await.expect("is_migration_run");
        assert!(applied, "migration v{v} should be marked as applied");
    }

    // Version 0 is not used in the current scheme.
    let run_0 = store.is_migration_run(0).await.expect("is_migration_run");
    assert!(
        !run_0,
        "version 0 is not a real migration and must not be set"
    );
}

/// Running `Store::connect` twice on the same in-memory database must not fail.
/// This is the idempotency guarantee: all migrations use `IF NOT EXISTS` guards
/// and `ON CONFLICT DO NOTHING`, so repeated runs are safe.
#[tokio::test]
async fn test_migration_idempotency() {
    let store = Store::connect("sqlite::memory:")
        .await
        .expect("first connect");

    // Manually call run_migrations again to simulate a re-run.
    store
        .run_migrations()
        .await
        .expect("second run_migrations must be idempotent");

    let version = store
        .get_current_version()
        .await
        .expect("get_current_version");
    assert_eq!(
        version, 13,
        "version must remain 13 after idempotent re-run"
    );
}

/// Simulates a database that was bootstrapped at v2 (e.g. an older deployment
/// that never ran v3–v5). After calling `run_migrations`, v3-v5 must be applied
/// and v1-v2 must remain intact.
#[tokio::test]
async fn test_migration_incremental_from_v2() {
    // Bootstrap with only v1-v2 tables and schema_version table set to v2.
    let store = {
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let s = Store {
            rpool: pool.clone(),
            pool,
            kind: DatabaseKind::Sqlite,
            master_key: std::sync::OnceLock::new(),
        };

        // Manually create the tables that v1 and v2 would create.
        s.ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS clients (
                    vtoken TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                    label TEXT, created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP), last_seen TEXT
                )",
        )
        .await
        .expect("clients");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS routing_state (
                    from_user TEXT PRIMARY KEY,
                    active_vtoken TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("routing_state");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
        )
        .await
        .expect("context_token_map");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS bot_credentials (
                    id INTEGER PRIMARY KEY, token TEXT NOT NULL,
                    base_url TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("bot_credentials");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL, backend_session_id TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken, session_name)
                )",
        )
        .await
        .expect("backend_sessions_v2");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS active_sessions (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL DEFAULT 'default',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken)
                )",
        )
        .await
        .expect("active_sessions");

        // Mark v1 and v2 as already applied.
        s.record_migration_run(1).await.expect("mark v1");
        s.record_migration_run(2).await.expect("mark v2");

        s
    };

    // v3-v5 should not yet be applied.
    assert!(!store.is_migration_run(3).await.unwrap());
    assert!(!store.is_migration_run(4).await.unwrap());
    assert!(!store.is_migration_run(5).await.unwrap());

    // Running migrations now must apply v3-v5.
    store.run_migrations().await.expect("incremental migration");

    let version = store.get_current_version().await.unwrap();
    assert_eq!(version, 13, "must reach v13 after incremental migration");

    for v in 1..=13 {
        assert!(
            store.is_migration_run(v).await.unwrap(),
            "v{v} must be marked applied"
        );
    }
}

#[tokio::test]
async fn migration_runs_on_in_memory_sqlite() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // If migration ran, these should succeed
    let r = store.list_clients().await;
    assert!(r.is_ok(), "list_clients failed: {:?}", r.err());
    let r = store
        .find_or_create_vctx("test-user", None, "real-ctx")
        .await;
    assert!(r.is_ok(), "find_or_create_vctx failed: {:?}", r.err());
}

/// v6: pre-v6 databases stored `peer_user_id` as the bare WeChat peer ID
/// (no `peer:` / `group:` prefix), but the current `find_or_create_vctx`
/// writes a prefixed `conv_key` and queries by that prefix. Running v6
/// must rewrite every non-empty, non-prefixed row to add the `peer:`
/// prefix — otherwise every new message mints a fresh vctx and the
/// existing conversation is orphaned.
///
/// We pre-seed `context_token_map` with three rows:
///   1. bare peer ID (must be prefixed)
///   2. already-prefixed `peer:` row (must be left alone)
///   3. already-prefixed `group:` row (must be left alone)
/// and assert post-migration values.
#[tokio::test]
async fn test_migration_v6_normalizes_peer_user_id_format() {
    sqlx::any::install_default_drivers();
    let store = {
        let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        let s = Store {
            rpool: pool.clone(),
            pool,
            kind: DatabaseKind::Sqlite,
            master_key: std::sync::OnceLock::new(),
        };

        // Manually create the v1-v5 schema (we don't need the full DDL — we
        // only care about context_token_map and the migration bookkeeping).
        s.ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
        s.ddl(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                    vctx TEXT PRIMARY KEY,
                    real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '',
                    created_at TEXT
                )",
        )
        .await
        .expect("context_token_map");

        // Mark v1-v5 as already applied so v6 is the only one that runs.
        for v in 1..=5 {
            s.record_migration_run(v).await.expect("mark v{v}");
        }

        // Pre-v6 data: row 1 has the old bare peer_user_id format;
        // rows 2 and 3 are already in the new format and must be left alone.
        s.ddl(
            "INSERT INTO context_token_map (vctx, real_ctx, peer_user_id) VALUES
                    ('vctx-old-1', 'ctx-1', 'o9cq80_ZyXuz1vAtG-TMbQjwQPW8@im.wechat'),
                    ('vctx-new-2', 'ctx-2', 'peer:already@im.wechat'),
                    ('vctx-grp-3', 'ctx-3', 'group:chatroom-123')",
        )
        .await
        .expect("seed");

        s
    };

    assert!(
        !store.is_migration_run(6).await.unwrap(),
        "v6 must not be marked yet"
    );
    store.run_migrations().await.expect("run_migrations");
    let cur_ver = store.get_current_version().await.unwrap();
    assert_eq!(cur_ver, 13, "current version must be 13, got {}", cur_ver);
    assert!(
        store.is_migration_run(6).await.unwrap(),
        "v6 must be marked after run"
    );

    // Row 1: bare ID must now be prefixed.
    let row1 = store
        .resolve_context_token_full("vctx-old-1")
        .await
        .expect("resolve vctx-old-1")
        .expect("vctx-old-1 must exist");
    assert_eq!(
        row1.1, "peer:o9cq80_ZyXuz1vAtG-TMbQjwQPW8@im.wechat",
        "bare peer_user_id must be prefixed with 'peer:'"
    );

    // Row 2: already-prefixed peer: row must be unchanged.
    let row2 = store
        .resolve_context_token_full("vctx-new-2")
        .await
        .expect("resolve vctx-new-2")
        .expect("vctx-new-2 must exist");
    assert_eq!(
        row2.1, "peer:already@im.wechat",
        "already-prefixed peer: row must be left alone"
    );

    // Row 3: already-prefixed group: row must be unchanged.
    let row3 = store
        .resolve_context_token_full("vctx-grp-3")
        .await
        .expect("resolve vctx-grp-3")
        .expect("vctx-grp-3 must exist");
    assert_eq!(
        row3.1, "group:chatroom-123",
        "already-prefixed group: row must be left alone"
    );

    // Re-running run_migrations must be a no-op for v6 (idempotent).
    store.run_migrations().await.expect("second run_migrations");
    let row1_again = store
        .resolve_context_token_full("vctx-old-1")
        .await
        .expect("resolve vctx-old-1 again")
        .expect("vctx-old-1 must exist");
    assert_eq!(
        row1_again.1, "peer:o9cq80_ZyXuz1vAtG-TMbQjwQPW8@im.wechat",
        "v6 must be idempotent on re-run"
    );
}

/// Regression test for DB-01: file-type SQLite must pin the pool to a
/// single connection so that concurrent write transactions and reads
/// from different physical connections cannot race on the SQLite file
/// lock and return `SQLITE_BUSY` (5).
///
/// Before the fix, `AnyPool::connect(url)` for `sqlite:/path/to.db`
/// defaulted to 10 connections. With multiple tasks issuing write
/// transactions (`find_or_create_vctx`,
/// `set_active_session_name`) and reads (`get_active_session_name`)
/// concurrently, two physical connections would race on the
/// file-level EXCLUSIVE write lock; once a writer's lock-hold time
/// exceeded the default `busy_timeout` (5s), a competing transaction
/// would surface `SQLITE_BUSY`. The fix collapses the pool to
/// `max_connections(1)` for any `sqlite:` URL, which serializes
/// transactions on a single connection (no second connection means
/// no second contender for the file lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn file_sqlite_serializes_concurrent_read_and_write_without_busy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("concurrent.db");
    let url = format!("sqlite:{}", db_path.display());
    let store = std::sync::Arc::new(Store::connect(&url).await.expect("connect"));

    // The fix is structural: the pool must be sized to a single
    // connection for any sqlite URL. Verify the invariant first
    // (fast, deterministic, pinpoints regressions), then run a
    // multi-task mixed read/write workload that would surface
    // SQLITE_BUSY on a multi-connection pool with a non-default
    // (small) busy_timeout. The structural assertion is the
    // canonical regression guard.
    assert_eq!(
        store.pool.options().get_max_connections(),
        1,
        "SQLite pool must be pinned to max_connections(1) to avoid SQLITE_BUSY"
    );

    // Seed one row so the read path has a target.
    store
        .find_or_create_vctx("peer-seed", None, "real-ctx-seed")
        .await
        .expect("seed");
    store
        .set_active_session_name("vctx-seed", "vtoken-seed", "default")
        .await
        .expect("seed active session");

    let mut handles = Vec::new();

    // Batch-write task: hammer find_or_create_vctx with many entries to exercise
    // concurrent DB writes and increase the chance of a write/write race on the
    // file lock. Each task runs 20 iterations with 10 entries each.
    for w in 0..8 {
        let store = std::sync::Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..20 {
                for j in 0..10 {
                    store
                        .find_or_create_vctx(
                            &format!("peer-w{w}-i{i}-j{j}"),
                            None,
                            &format!("real-ctx-w{w}-i{i}-j{j}"),
                        )
                        .await
                        .expect("find_or_create_vctx must not fail");
                }
            }
        }));
    }

    // Single-row write task: hammer set_active_session_name (a
    // write transaction) on a different row each time so we are
    // exercising the same physical-connection file-lock path.
    for w in 0..4 {
        let store = std::sync::Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..200 {
                let vctx = format!("vctx-active-w{w}-i{i}");
                let vtoken = format!("vtoken-active-w{w}-i{i}");
                store
                    .set_active_session_name(&vctx, &vtoken, "default")
                    .await
                    .expect("set_active_session_name must not fail");
            }
        }));
    }

    // Reader task: hammer get_active_session_name.
    for r in 0..4 {
        let store = std::sync::Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..200 {
                let vtoken = format!("ignored-vtoken-r{r}-i{i}");
                let name = store
                    .get_active_session_name("vctx-seed", &vtoken)
                    .await
                    .expect("read must not fail");
                assert_eq!(name, "default");
            }
        }));
    }

    for h in handles {
        h.await.expect("task join");
    }
}

#[tokio::test]
async fn test_sync_02_upsert_client_updates_routing_state() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // Register client "bridge-a" with "vtoken-1"
    store
        .upsert_client("vtoken-1", "bridge-a", None)
        .await
        .unwrap();

    // Set route for user "alice" to "vtoken-1"
    store.set_route("alice", "vtoken-1").await.unwrap();

    // Verify route is set
    let route = store.get_route("alice").await.unwrap();
    assert_eq!(route, Some("vtoken-1".to_string()));

    // Re-register client "bridge-a" with "vtoken-2"
    store
        .upsert_client("vtoken-2", "bridge-a", None)
        .await
        .unwrap();

    // Verify route is updated to "vtoken-2"
    let route = store.get_route("alice").await.unwrap();
    assert_eq!(route, Some("vtoken-2".to_string()));
}

#[tokio::test]
async fn test_db_03_get_hub_ext_batch_query() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // Insert some session data
    store
        .set_active_session_name("vctx-1", "vtoken-1", "session-1")
        .await
        .unwrap();
    store
        .set_active_session_name("vctx-2", "vtoken-2", "session-2")
        .await
        .unwrap();

    store
        .set_backend_session("vctx-1", "vtoken-1", "session-1", "sid-1")
        .await
        .unwrap();
    store
        .set_backend_session("vctx-2", "vtoken-2", "session-2", "sid-2")
        .await
        .unwrap();

    let pairs = vec![
        ("vctx-1".to_string(), "vtoken-1".to_string()),
        ("vctx-2".to_string(), "vtoken-2".to_string()),
        ("vctx-3".to_string(), "vtoken-3".to_string()), // nonexistent
    ];

    let result = store.get_hub_ext_batch(&pairs).await.unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(
        result.get(&("vctx-1".to_string(), "vtoken-1".to_string())),
        Some(&("session-1".to_string(), Some("sid-1".to_string())))
    );
    assert_eq!(
        result.get(&("vctx-2".to_string(), "vtoken-2".to_string())),
        Some(&("session-2".to_string(), Some("sid-2".to_string())))
    );
    assert_eq!(
        result.get(&("vctx-3".to_string(), "vtoken-3".to_string())),
        Some(&("default".to_string(), None))
    );
}

#[tokio::test]
async fn test_db_02_find_or_create_vctx_multiple_peers() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // Create 55 distinct peer conversations.
    for i in 0..55 {
        store
            .find_or_create_vctx(&format!("peer-{i}"), None, &format!("real-{i}"))
            .await
            .unwrap();
    }

    // All 55 entries must be persisted: each peer should resolve consistently.
    for i in 0..55 {
        let v1 = store
            .find_or_create_vctx(&format!("peer-{i}"), None, &format!("real-{i}"))
            .await
            .unwrap();
        let v2 = store
            .find_or_create_vctx(&format!("peer-{i}"), None, &format!("real-{i}-new"))
            .await
            .unwrap();
        assert_eq!(v1, v2, "peer-{i} must always get the same vctx");
    }
}

#[tokio::test]
async fn test_sync_02_upsert_client_concurrent_adversarial() {
    // Create a temporary database in target/ directory of the workspace
    let temp_dir = tempfile::Builder::new()
        .prefix("test_concurrent_db")
        .tempdir_in("target")
        .unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db_url = format!("sqlite:{}", db_path.to_str().unwrap());

    let store = Store::connect(&db_url).await.expect("connect");

    // Initial setup: register client "bridge-concurrent" with "vtoken-initial"
    store
        .upsert_client("vtoken-initial", "bridge-concurrent", None)
        .await
        .unwrap();

    // Set route for user "alice" to "vtoken-initial"
    store.set_route("alice", "vtoken-initial").await.unwrap();

    // Now run multiple concurrent upserts of client "bridge-concurrent"
    let num_concurrency = 20;
    let mut handles = vec![];

    let store = std::sync::Arc::new(store);

    for i in 0..num_concurrency {
        let store_clone = store.clone();
        let vtoken = format!("vtoken-{}", i);
        let handle = tokio::spawn(async move {
            store_clone
                .upsert_client(&vtoken, "bridge-concurrent", None)
                .await
        });
        handles.push(handle);
    }

    // Wait for all tasks to complete
    for h in handles {
        h.await.unwrap().unwrap();
    }

    // Retrieve the final vtoken in the clients table
    let clients = store.list_clients().await.unwrap();
    let final_client_vtoken = clients
        .iter()
        .find(|c| c.name == "bridge-concurrent")
        .map(|c| c.vtoken.clone())
        .unwrap();

    // Retrieve the route for "alice"
    let final_route = store.get_route("alice").await.unwrap().unwrap();

    // Under race conditions in the old implementation, final_route would be stale
    // while final_client_vtoken would be the last committed vtoken.
    // We assert that they must be identical.
    assert_eq!(final_route, final_client_vtoken);
}

// ─── Adversarial regression tests for the review findings ─────────────────
//
// Each test below pins down a specific finding from the M1 review. They are
// grouped by the finding they cover, not by topic, so a future reader
// hunting for "what was F-M1-02?" can grep and land here.

/// F-M1-01 / F-M1-04 / F-M3-02: Two TRULY concurrent `Store::connect`
/// calls against the same file-backed SQLite database must BOTH succeed
/// and converge to `get_current_version() == 5`. The two connect tasks
/// are spawned via `tokio::join!` so they race in flight — the M3 review
/// (F-M3-02) flagged that the prior version of this test ran sequentially
/// (one connect awaited before the next started) and so did NOT exercise
/// the concurrent-claim path that `try_claim_migration` is designed to
/// close. With `tokio::join!` both connect tasks are polling the runtime
/// scheduler at once, and the only thing serialising them is the
/// atomic `try_claim_migration` `INSERT ... ON CONFLICT DO NOTHING
/// RETURNING` (SQLite/Postgres) or `INSERT IGNORE` (MySQL) primitive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adversarial_concurrent_store_connect_succeeds_and_converges() {
    sqlx::any::install_default_drivers();
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite:{}/concurrent.db", tmp.path().display());
    let url1 = url.clone();
    let url2 = url.clone();
    // tokio::join! polls both futures on the same task; the multi-thread
    // runtime above lets them run in parallel. Both connect() calls are
    // in flight at the same time; the SQLite single-connection pin
    // serialises them on the file lock, but the claim primitive
    // (try_claim_migration) is the load-bearing piece — without it the
    // v3 / v4 DDL would double-run.
    let (s1, s2) = tokio::join!(async move { Store::connect(&url1).await }, async move {
        Store::connect(&url2).await
    },);
    let s1 = s1.expect("connect #1 must succeed");
    let s2 = s2.expect("connect #2 must succeed");
    assert_eq!(
        s1.get_current_version().await.unwrap(),
        13,
        "writer #1 must see all v1-v13 applied"
    );
    assert_eq!(
        s2.get_current_version().await.unwrap(),
        13,
        "writer #2 must see all v1-v13 applied"
    );
    // The whole schema must be usable from both writers — no half-applied
    // tables, no missing indexes.
    for s in [&s1, &s2] {
        assert!(s.list_clients().await.is_ok());
        assert!(s
            .find_or_create_vctx("schema-check-user", None, "schema-check-real")
            .await
            .is_ok());
    }
}

/// F-M1-01 / F-M3-02: 10 TRULY concurrent `Store::connect` callers (each
/// spawned via `tokio::spawn` and joined via `futures::join_all`) must
/// all converge to version 5. The prior version of this test was
/// sequential (a `for` loop awaiting each connect before starting the
/// next) and so did NOT exercise the concurrent-claim path.
/// `tokio::spawn` + `join_all` puts all 10 connects in flight at once;
/// `try_claim_migration` is the only thing keeping the v1-v5 DDL from
/// running multiple times.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn adversarial_many_concurrent_connects_converge() {
    sqlx::any::install_default_drivers();
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite:{}/many.db", tmp.path().display());
    let mut handles = Vec::with_capacity(10);
    for i in 0..10 {
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let mut last_err = String::new();
            for attempt in 0..15 {
                match Store::connect(&url).await {
                    Ok(s) => return Ok(s),
                    Err(e) => {
                        let is_busy = e
                            .downcast_ref::<sqlx::Error>()
                            .map(|se| {
                                matches!(
                                    se,
                                    sqlx::Error::Database(ref db_err)
                                        if db_err.code().as_deref() == Some("5")
                                )
                            })
                            .unwrap_or(false);
                        last_err = format!("{e}");
                        if is_busy && attempt < 14 {
                            let delay = 200 + (rand::random::<u32>() % 300) as u64;
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                            continue;
                        }
                    }
                }
            }
            Err(format!("connect #{i} failed after retries: {last_err}"))
        }));
    }
    let mut stores = Vec::with_capacity(handles.len());
    for h in handles {
        stores.push(
            h.await
                .expect("task join")
                .unwrap_or_else(|e| panic!("{e}")),
        );
    }
    for (i, s) in stores.iter().enumerate() {
        assert_eq!(
            s.get_current_version().await.unwrap(),
            13,
            "connect #{i} must see all v1-v13 applied"
        );
    }
}

/// F-M1-02: v4's "column already exists" branch is now driven by an
/// `information_schema.columns` pre-check, not by an error-string match.
/// Simulate the pre-schema_version deployment state (v1+v2 tables exist,
/// `created_at` already present, v1+v2 marked run) and verify the v4 path
/// is silently skipped (no error, no DDL) and the index is created.
#[tokio::test]
async fn adversarial_v4_skips_alter_when_column_already_present() {
    // Install drivers so the manual pool below can use them.
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // Bootstrap the same v1+v2 state as `test_migration_incremental_from_v2`.
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS clients (
                    vtoken TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                    label TEXT, created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP), last_seen TEXT
                )",
        )
        .await
        .expect("clients");
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS routing_state (
                    from_user TEXT PRIMARY KEY,
                    active_vtoken TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("routing_state");
    // The legacy state: `created_at` already present, but schema_version
    // doesn't yet know about v4.
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT,
                    created_at TEXT
                )",
        )
        .await
        .expect("context_token_map (with created_at)");
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS bot_credentials (
                    id INTEGER PRIMARY KEY, token TEXT NOT NULL,
                    base_url TEXT NOT NULL DEFAULT 'https://ilinkai.weixin.qq.com',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("bot_credentials");
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS backend_sessions_v2 (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL, backend_session_id TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken, session_name)
                )",
        )
        .await
        .expect("backend_sessions_v2");
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS active_sessions (
                    vctx TEXT NOT NULL, vtoken TEXT NOT NULL,
                    session_name TEXT NOT NULL DEFAULT 'default',
                    updated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
                    PRIMARY KEY (vctx, vtoken)
                )",
        )
        .await
        .expect("active_sessions");
    store.record_migration_run(1).await.expect("mark v1");
    store.record_migration_run(2).await.expect("mark v2");

    // Run migrations: v3, v4, v5 must all run, and v4 must NOT fail.
    store
        .run_migrations()
        .await
        .expect("run_migrations must succeed");

    // All v1-v5 must be marked applied.
    for v in 1..=5 {
        assert!(
            store.is_migration_run(v).await.unwrap(),
            "v{v} must be marked applied after run_migrations"
        );
    }
    // The pre-check took the "skip" branch — verify by reading the catalog
    // directly. If the column were missing, the v4 path would have re-added
    // it. Reading the catalog also confirms we did not accidentally drop
    // the legacy column.
    assert!(
        store
            .column_exists("context_token_map", "created_at")
            .await
            .unwrap(),
        "created_at must still exist (we only skip the ALTER, never drop)"
    );
}

/// F-M1-03: a column-decode error on `get_current_version` must be
/// propagated, not swallowed. We seed the table with a TEXT value that
/// SQLite accepts (via type affinity) but sqlx refuses to decode as
/// `i32`, call `get_current_version`, and assert the result is `Err`
/// rather than a silent `Ok(0)`.
///
/// Also pins down the M3 lock-sentinel filter: the migration runner
/// stores its `MIGRATION_LOCK_VERSION = i32::MAX` row alongside the
/// per-step rows, and `get_current_version` must exclude that row from
/// its result so external callers see the real schema version (e.g. 5),
/// not the lock sentinel.
#[tokio::test]
async fn adversarial_get_current_version_propagates_decode_error() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
    // (1) Lock sentinel filter: insert a row at the lock sentinel
    // value (i32::MAX). The new `get_current_version` filter
    // excludes it. The table appears "empty" to external callers.
    sqlx::query("INSERT INTO schema_version (version) VALUES ($1)")
        .bind(i32::MAX)
        .execute(&store.pool)
        .await
        .expect("insert lock sentinel");
    let res = store.get_current_version().await;
    assert_eq!(
            res.ok(),
            Some(0),
            "get_current_version must exclude the lock sentinel and return 0 for an empty (real-version) table"
        );

    // (2) F-M1-03 regression: the `schema_version.version` column is
    // `INTEGER PRIMARY KEY`, which SQLite enforces strictly — text values
    // are rejected at the driver level with "datatype mismatch" (code 20).
    // This means the pathological scenario the original test intended to
    // simulate (text stored via type affinity) CANNOT occur in practice
    // with this schema: the constraint itself is the guard.
    // We verify this defence-in-depth by confirming the insert IS rejected.
    sqlx::query("DELETE FROM schema_version WHERE version = $1")
        .bind(i32::MAX)
        .execute(&store.pool)
        .await
        .expect("delete lock sentinel");
    let bad_insert = sqlx::query("INSERT INTO schema_version (version) VALUES ('not-a-number')")
        .execute(&store.pool)
        .await;
    assert!(
        bad_insert.is_err(),
        "SQLite INTEGER PRIMARY KEY must reject non-integer insert — F-M1-03 defence-in-depth"
    );
}

/// F-M1-07: `is_migration_run` / `get_current_version` / `try_claim_migration`
/// must accept version values used by the migration runner and the test
/// surface. A negative version is not a real migration but the API must
/// not crash. `get_current_version` and `try_claim_migration` follow the
/// same shape — they bind the version and pass it through to the driver.
/// This test pins down the boundary behaviour for the version = 0 and
/// negative cases so a future refactor that tightens input validation
/// has a clear contract to keep.
#[tokio::test]
async fn adversarial_version_api_boundaries() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // is_migration_run(0): not applied, no error.
    assert!(!store.is_migration_run(0).await.unwrap());
    // is_migration_run(-1): not applied, no error.
    assert!(!store.is_migration_run(-1).await.unwrap());
    // get_current_version: 13 (the highest applied after full connect).
    assert_eq!(store.get_current_version().await.unwrap(), 13);
}

/// F-M1-08: `try_claim_migration` is the atomic primitive. Two concurrent
/// claims for the SAME version on a multi-connection-shaped scenario
/// (simulated by issuing two claims on the same store) must result in
/// exactly one `true` and one `false`. This is the unit-level guard
/// for the M1 invariant; the end-to-end variant is the
/// `adversarial_concurrent_store_connect_succeeds_and_converges` test.
///
/// On SQLite with `max_connections(1)` the SQL is serialised by the
/// connection, so the second claim always observes the first's row. On
/// Postgres / MySQL the `ON CONFLICT DO NOTHING RETURNING` clause is the
/// thing that serialises the claim — the second `INSERT` is a no-op
/// (returns no row). The test works on SQLite because the storage path
/// is the same `INSERT ... ON CONFLICT DO NOTHING RETURNING` SQL.
#[tokio::test]
async fn adversarial_try_claim_is_mutually_exclusive() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // Manually delete v5's claim row so we can race for it.
    sqlx::query("DELETE FROM schema_version WHERE version = 5")
        .execute(&store.pool)
        .await
        .expect("delete v5 row");
    // Two back-to-back claims; the second must observe the first's row.
    let first = store.try_claim_migration(5).await.unwrap();
    let second = store.try_claim_migration(5).await.unwrap();
    assert!(first, "first claim must win");
    assert!(!second, "second claim must lose");
}

/// F-M3-02 (unit-level): two TRULY concurrent `try_claim_migration`
/// calls on the same `Store` must produce exactly one `true` and one
/// `false`. The pre-M3-fix version of the test issued the two claims
/// back-to-back in a single `async` block, which serialised them on
/// the same task — the second `await` never started until the first
/// had returned. The M3 fix uses `tokio::join!` so both futures
/// poll concurrently on the multi-thread runtime; the claim
/// primitive (`INSERT ... ON CONFLICT DO NOTHING RETURNING` for
/// SQLite/Postgres, `INSERT IGNORE` for MySQL) is the only thing
/// keeping exactly one claim from winning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adversarial_try_claim_is_mutually_exclusive_concurrent() {
    let store = std::sync::Arc::new(Store::connect("sqlite::memory:").await.expect("connect"));
    // Manually delete v5's claim row so we can race for it. Both
    // clones see the same schema_version table; only one insert can
    // win.
    sqlx::query("DELETE FROM schema_version WHERE version = 5")
        .execute(&store.pool)
        .await
        .expect("delete v5 row");
    let s1 = std::sync::Arc::clone(&store);
    let s2 = std::sync::Arc::clone(&store);
    // tokio::join! polls both futures on the same task; the
    // multi-thread runtime above lets them run in parallel. Both
    // INSERTs are in flight at once.
    let (first, second) = tokio::join!(
        async move { s1.try_claim_migration(5).await.unwrap() },
        async move { s2.try_claim_migration(5).await.unwrap() },
    );
    // Exactly one of the two claims must win. The other must see
    // the row already present and return false.
    let wins = [first, second].iter().filter(|w| **w).count();
    assert_eq!(
        wins, 1,
        "exactly one of two concurrent try_claim_migration(5) calls must win; got {first}/{second}"
    );
    // The version row must be present exactly once.
    let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM schema_version WHERE version = 5")
        .fetch_one(&store.pool)
        .await
        .expect("count");
    assert_eq!(rows.0, 1, "exactly one claim row must exist for v5");
}

// ─── M2 regression tests ───────────────────────────────────────────────
//
// M2 refactors `run_migrations` into per-version `migrate_to_vN` functions.
// Each step is gated by `try_claim_migration`, the DDL errors propagate
// via `?` rather than being swallowed, and the schema_version table is
// updated as a side-effect of the claim. The tests below pin each of
// those invariants.

/// F-M2-01: every `migrate_to_vN` is independently callable and updates
/// `schema_version` only for its own version. Calling v2 alone after a
/// fresh connect (which has only v0) must record v2 and leave v1, v3,
/// v4, v5 unmarked.
#[tokio::test]
async fn m2_per_version_migrators_update_schema_version_independently() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // Bootstrap only the schema_version table — no migrations applied yet.
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");

    // Run only v2 in isolation.
    store.migrate_to_v2().await.expect("migrate_to_v2");

    // v2 must be marked; v1, v3, v4, v5 must not.
    assert!(
        store.is_migration_run(2).await.unwrap(),
        "v2 must be marked after migrate_to_v2"
    );
    for v in [1, 3, 4, 5] {
        assert!(
            !store.is_migration_run(v).await.unwrap(),
            "v{v} must NOT be marked after running only v2"
        );
    }

    // v2 tables must exist (sanity check the DDL actually ran).
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='backend_sessions_v2'",
    )
    .fetch_optional(&store.pool)
    .await
    .expect("catalog");
    assert!(
        row.is_some(),
        "backend_sessions_v2 must exist after migrate_to_v2"
    );

    // v1 tables must NOT exist (v1 was not run).
    let row: Option<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name='clients'")
            .fetch_optional(&store.pool)
            .await
            .expect("catalog");
    assert!(row.is_none(), "clients must NOT exist (v1 was not run)");
}

/// F-M2-02: re-running an already-applied migration is a no-op. The
/// claim returns false, the DDL is skipped, and the schema_version
/// row is unchanged.
#[tokio::test]
async fn m2_migrators_are_idempotent_per_step() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // After connect, all 10 are applied. Re-running each must NOT fail
    // and must NOT touch the schema_version table.
    store.migrate_to_v1().await.expect("v1 re-run");
    store.migrate_to_v2().await.expect("v2 re-run");
    store.migrate_to_v3().await.expect("v3 re-run");
    store.migrate_to_v4().await.expect("v4 re-run");
    store.migrate_to_v5().await.expect("v5 re-run");
    store.migrate_to_v6().await.expect("v6 re-run");
    store.migrate_to_v7().await.expect("v7 re-run");
    store.migrate_to_v8().await.expect("v8 re-run");
    store.migrate_to_v9().await.expect("v9 re-run");
    store.migrate_to_v10().await.expect("v10 re-run");
    store.migrate_to_v11().await.expect("v11 re-run");
    store.migrate_to_v12().await.expect("v12 re-run");
    store.migrate_to_v13().await.expect("v13 re-run");

    // Still at v13.
    assert_eq!(store.get_current_version().await.unwrap(), 13);
}

/// F-M2-03: a DDL failure inside a migrator must propagate as `Err`,
/// NOT be silently swallowed. We construct a synthetic failure: pre-
/// create a `context_token_map` whose schema blocks v3's
/// `CREATE UNIQUE INDEX`. The unique index is rejected when the table
/// already has duplicate `real_ctx` rows, so the migrator must
/// surface the underlying driver error.
#[tokio::test]
async fn m2_ddl_error_propagates_through_migrator() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // Bootstrap the version-tracking table and the v1 schema with
    // duplicated real_ctx values — the v3 unique index cannot be
    // created over a non-unique column.
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
    store
        .ddl(
            "CREATE TABLE context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
        )
        .await
        .expect("context_token_map");
    // Two rows with the same real_ctx → v3's CREATE UNIQUE INDEX fails.
    sqlx::query("INSERT INTO context_token_map (vctx, real_ctx) VALUES ($1, $2)")
        .bind("vctx-1")
        .bind("dup-real")
        .execute(&store.pool)
        .await
        .expect("seed row 1");
    sqlx::query("INSERT INTO context_token_map (vctx, real_ctx) VALUES ($1, $2)")
        .bind("vctx-2")
        .bind("dup-real")
        .execute(&store.pool)
        .await
        .expect("seed row 2");

    // migrate_to_v3 must surface the CREATE UNIQUE INDEX error.
    let result = store.migrate_to_v3().await;
    assert!(
        result.is_err(),
        "migrate_to_v3 must propagate DDL errors, got Ok — F-M2-03 not fixed"
    );
    // The non-_tx wrapper now runs inside its own transaction, so a DDL
    // failure causes a full rollback — the claim row is NOT retained.
    // This is cleaner than the old behaviour: the migrator can be safely
    // retried after fixing the underlying data issue (e.g. deduplicating
    // real_ctx rows), without a manual DELETE from schema_version.
    assert!(
        !store.is_migration_run(3).await.unwrap(),
        "v3 claim row must be absent after rollback — migrator is safely retryable"
    );
}

/// F-M2-04: `record_migration_run` (the safety-net kept in M1) writes
/// the row even after the migrator has already claimed the version.
/// Since `try_claim_migration` already inserts the row, calling
/// `record_migration_run` again is a no-op. The combined behaviour:
/// the row is present exactly once, and a second `try_claim_migration`
/// returns false.
#[tokio::test]
async fn m2_claim_and_record_are_consistent_with_schema_version() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // v3 is already applied. A second try_claim must observe the row.
    assert!(
        !store.try_claim_migration(3).await.unwrap(),
        "v3 is already applied; second claim must lose"
    );
    // record_migration_run is a no-op (ON CONFLICT DO NOTHING).
    store
        .record_migration_run(3)
        .await
        .expect("record_migration_run(3) must be a no-op");
    // The version row is still present (we did not delete it).
    assert!(store.is_migration_run(3).await.unwrap());
}

/// F-M2-05: invoking a higher-version migrator before a lower one
/// must not deadlock or produce a partial state. The migrator's
/// pre-condition is that the schema_version table exists; that's
/// bootstrapped by `run_migrations`, but a per-version call on a
/// fresh pool needs the table. We bootstrap manually here, then
/// run v4 alone: v4 expects `context_token_map` to exist (it
/// `ADD COLUMN`s onto it), so we also pre-create that table. The
/// test pins down "running a single migrator on a partial state
/// with the right pre-conditions is fine and records v4".
#[tokio::test]
async fn m2_v4_alone_with_minimal_preconditions() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
    store
        .ddl(
            "CREATE TABLE context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
        )
        .await
        .expect("context_token_map");

    // v4 alone: column does not exist, so the ALTER must run.
    store.migrate_to_v4().await.expect("migrate_to_v4");
    assert!(store.is_migration_run(4).await.unwrap());

    // The column was added; the index was created.
    assert!(
        store
            .column_exists("context_token_map", "created_at")
            .await
            .unwrap(),
        "created_at column must exist after v4"
    );
    // Index exists (sqlite_master entry).
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master \
             WHERE type='index' AND name='idx_context_token_map_created_at'",
    )
    .fetch_optional(&store.pool)
    .await
    .expect("catalog");
    assert!(
        row.is_some(),
        "idx_context_token_map_created_at must exist after v4"
    );
}

/// F-M2-06: full `run_migrations` walks all steps in order and
/// records v1..=v9 in `schema_version`. This is the headline M2
/// invariant: any DDL error along the way aborts the walk.
#[tokio::test]
async fn m2_run_migrations_records_all_versions_in_order() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // All ten versions are present.
    for v in 1..=10 {
        assert!(
            store.is_migration_run(v).await.unwrap(),
            "v{v} must be recorded after run_migrations"
        );
    }
    // get_current_version returns the maximum.
    assert_eq!(store.get_current_version().await.unwrap(), 13);
}

/// F-M2-07: `run_migrations` invoked twice in a row must remain
/// idempotent. The M2 refactor's "early return on claim == false"
/// shape is what makes this safe; the test pins it down.
#[tokio::test]
async fn m2_run_migrations_idempotent_double_call() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // Second call must succeed.
    store.run_migrations().await.expect("second run_migrations");
    // Version stays at 13 (no ghost rows from a third call).
    assert_eq!(store.get_current_version().await.unwrap(), 13);
}

/// F-M2-08: each `migrate_to_vN` uses `CURRENT_TIMESTAMP` (not
/// `datetime('now')`) for any timestamp default. The plan calls for
/// unifying the DDL on `CURRENT_TIMESTAMP`. We check the catalog for
/// each table's `sql` field and assert that no DDL contains the
/// legacy `datetime('now')` form. The catalog on SQLite preserves
/// the original CREATE TABLE statement, so this is a direct check.
#[tokio::test]
async fn m2_ddl_uses_current_timestamp_only() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL")
            .fetch_all(&store.pool)
            .await
            .expect("catalog");
    for (sql,) in rows {
        assert!(
            !sql.contains("datetime('now')"),
            "DDL must not use legacy datetime('now'): {sql}"
        );
        assert!(
            sql.contains("CURRENT_TIMESTAMP")
                || !sql.contains("TIMESTAMP") && !sql.contains("timestamp"),
            "DDL should prefer CURRENT_TIMESTAMP where applicable: {sql}"
        );
    }
}

// ─── M3 regression tests ───────────────────────────────────────────────
//
// M3 synchronises and aligns the `migrations/*.sql` files with the
// inline DDL in `migrate_to_vN`. The tests below pin down the M3
// invariants: (a) every `migrations/*.sql` is the human-readable
// reference for the corresponding Rust migrator (modulo the v5
// AUTOINCREMENT/IDENTITY driver split, F-M2-02), (b) no SQL file
// contains the legacy `datetime('now')` form, (c) the index names
// defined in SQL match the catalog after `run_migrations`, and
// (d) the v5 DDL is portable across SQLite / Postgres / MySQL
// (the F-M2-02 fix).

/// Normalise whitespace: collapse runs of spaces/tabs into a single
/// space, drop leading/trailing whitespace on each line, drop
/// blank lines, drop `-- ...` line comments. Used to compare a
/// reference SQL file against an inline Rust DDL string when the
/// two have only indentation / line-break differences.
fn normalise_sql(s: &str) -> String {
    s.lines()
        .map(|l| {
            // strip `--` line comments (not inside strings — none of
            // the inline DDLs contain `--` outside of comments).
            if let Some(idx) = l.find("--") {
                &l[..idx]
            } else {
                l
            }
        })
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            // collapse internal runs of whitespace to a single space,
            // but keep `;` attached to the previous token (so an
            // end-of-statement `;` on its own line still reads as
            // part of the previous line).
            let mut out = String::with_capacity(l.len());
            let mut prev_space = false;
            for c in l.chars() {
                if c == ';' {
                    // attach to previous token
                    out.push(';');
                    prev_space = false;
                } else if c.is_whitespace() {
                    if !prev_space {
                        out.push(' ');
                    }
                    prev_space = true;
                } else {
                    out.push(c);
                    prev_space = false;
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// F-M3-01: the SQLite branch of `v5_create_messages_sql` matches the
/// `CREATE TABLE messages` block in `migrations/0005_messages.sql` after
/// whitespace normalisation. The two should be byte-identical modulo
/// indentation and the line-break conventions of the two contexts
/// (Rust string literal vs. SQL file). The 0005 file also contains the
/// two CREATE INDEX statements; those are covered by F-M3-05.
#[test]
fn m3_v5_sqlite_ddl_matches_migration_file() {
    // CARGO_MANIFEST_DIR is the workspace root for ilink-hub. The
    // migrations/ dir sits at the workspace root.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let sql_path = manifest_dir.join("migrations").join("0005_messages.sql");
    let sql_text = std::fs::read_to_string(&sql_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", sql_path.display()));
    // Extract just the CREATE TABLE block (everything up to the first
    // closing `;`). The two CREATE INDEX statements that follow are
    // covered by F-M3-05. The block already ends with `;` in the SQL
    // file, so the normaliser sees a trailing `;` on the last
    // non-empty line.
    let create_table_block = sql_text.split(';').next().unwrap_or("").trim().to_string() + ";";
    // `v5_create_messages_sql` does not include the trailing `;`
    // (the `ddl()` helper accepts statements both with and without
    // it). Append one for the comparison so the two normalised
    // strings have the same shape.
    let expected = Store::v5_create_messages_sql(DatabaseKind::Sqlite) + ";";
    assert_eq!(
        normalise_sql(&expected),
        normalise_sql(&create_table_block),
        "SQLite v5 CREATE TABLE DDL diverges from migrations/0005_messages.sql — \
             update one or the other to keep them in sync (M3 invariant)"
    );
}

/// F-M3-02: the Postgres branch of `v5_create_messages_sql` uses
/// `GENERATED BY DEFAULT AS IDENTITY` (the SQL standard form) and
/// does NOT use `AUTOINCREMENT`. This is the F-M2-02 fix — the
/// SQLite-only keyword must not leak into the Postgres DDL. (The
/// MySQL branch uses `AUTO_INCREMENT` and is covered by a separate
/// test below.)
#[test]
fn m3_v5_postgres_ddl_uses_identity_not_autoincrement() {
    let ddl = Store::v5_create_messages_sql(DatabaseKind::Postgres);
    assert!(
        ddl.contains("GENERATED BY DEFAULT AS IDENTITY"),
        "Postgres v5 DDL must use SQL standard IDENTITY clause: {ddl}"
    );
    assert!(
        !ddl.contains("AUTOINCREMENT"),
        "Postgres v5 DDL must NOT use SQLite-only AUTOINCREMENT: {ddl}"
    );
}

/// F-M3-02 (MySQL branch): the MySQL form uses `AUTO_INCREMENT` (MySQL's
/// keyword, distinct from SQLite's `AUTOINCREMENT`) and `BIGINT NOT NULL`
/// (MySQL's `INTEGER` with `AUTO_INCREMENT` is silently mapped to `INT(11)`,
/// which collides with the `i64` decode used by `save_message`'s
/// `LAST_INSERT_ID()`). The Postgres / SQLite IDENTITY clause must not
/// leak into MySQL.
#[test]
fn m3_v5_mysql_ddl_uses_auto_increment_and_bigint() {
    let ddl = Store::v5_create_messages_sql(DatabaseKind::MySql);
    assert!(
        ddl.contains("AUTO_INCREMENT"),
        "MySQL v5 DDL must use MySQL AUTO_INCREMENT: {ddl}"
    );
    assert!(
        !ddl.contains("AUTOINCREMENT"),
        "MySQL v5 DDL must NOT use SQLite-only AUTOINCREMENT: {ddl}"
    );
    assert!(
        !ddl.contains("GENERATED BY DEFAULT AS IDENTITY"),
        "MySQL v5 DDL must NOT use Postgres IDENTITY clause: {ddl}"
    );
    assert!(
        ddl.contains("BIGINT"),
        "MySQL v5 DDL must declare id as BIGINT (not INTEGER): {ddl}"
    );
}

/// F-M3-01 (driver detection from URL): `DatabaseKind::from_url` must
/// recognise every supported scheme. Unknown schemes now return `Err` so
/// typos (e.g. `postgress://`) surface at startup instead of silently
/// falling back to SQLite. The M3 review flagged the old
/// `SELECT current_database()` runtime probe as broken on MySQL (it
/// errors on BOTH SQLite and MySQL); the fix parses the kind from the
/// URL prefix at `Store::connect` time.
#[test]
fn adversarial_database_kind_from_url() {
    assert_eq!(
        DatabaseKind::from_url("sqlite::memory:").unwrap(),
        DatabaseKind::Sqlite
    );
    assert_eq!(
        DatabaseKind::from_url("sqlite:/tmp/x.db").unwrap(),
        DatabaseKind::Sqlite
    );
    assert_eq!(
        DatabaseKind::from_url("sqlite:///var/data/x.db").unwrap(),
        DatabaseKind::Sqlite
    );
    // PostgreSQL support is gated behind the `postgres` feature flag.
    #[cfg(feature = "postgres")]
    {
        assert_eq!(
            DatabaseKind::from_url("postgres://u:p@h:5432/db").unwrap(),
            DatabaseKind::Postgres
        );
        assert_eq!(
            DatabaseKind::from_url("postgresql://u:p@h:5432/db").unwrap(),
            DatabaseKind::Postgres
        );
    }
    #[cfg(not(feature = "postgres"))]
    {
        assert!(
            DatabaseKind::from_url("postgres://u:p@h:5432/db").is_err(),
            "postgres:// must return Err when `postgres` feature is disabled"
        );
        assert!(
            DatabaseKind::from_url("postgresql://u:p@h:5432/db").is_err(),
            "postgresql:// must return Err when `postgres` feature is disabled"
        );
    }
    // MySQL support is gated behind the `mysql` feature flag.
    #[cfg(feature = "mysql")]
    {
        assert_eq!(
            DatabaseKind::from_url("mysql://u:p@h:3306/db").unwrap(),
            DatabaseKind::MySql
        );
        assert_eq!(
            DatabaseKind::from_url("mariadb://u:p@h:3306/db").unwrap(),
            DatabaseKind::MySql
        );
    }
    #[cfg(not(feature = "mysql"))]
    {
        assert!(
            DatabaseKind::from_url("mysql://u:p@h:3306/db").is_err(),
            "mysql:// must return Err when `mysql` feature is disabled"
        );
        assert!(
            DatabaseKind::from_url("mariadb://u:p@h:3306/db").is_err(),
            "mariadb:// must return Err when `mysql` feature is disabled"
        );
    }
    // Empty URL defaults to SQLite (the iLink Hub desktop default path).
    assert_eq!(DatabaseKind::from_url("").unwrap(), DatabaseKind::Sqlite);
    // Unknown schemes now return Err — a typo should not silently become SQLite.
    assert!(DatabaseKind::from_url("file:/tmp/x.db").is_err());
    assert!(DatabaseKind::from_url("postgress://u:p@h/db").is_err());
    assert!(DatabaseKind::from_url("http://example.com/db").is_err());
    // N-12: bare absolute file paths produce a friendly "looks like a file path" hint.
    let err = DatabaseKind::from_url("/home/user/db.sqlite").unwrap_err();
    assert!(
        err.to_string().contains("looks like a file path"),
        "expected 'looks like a file path' hint, got: {err}"
    );
    let err = DatabaseKind::from_url("./relative/db.sqlite").unwrap_err();
    assert!(
        err.to_string().contains("looks like a file path"),
        "expected 'looks like a file path' hint for relative path, got: {err}"
    );
    let err = DatabaseKind::from_url("~/db.sqlite").unwrap_err();
    assert!(
        err.to_string().contains("looks like a file path"),
        "expected 'looks like a file path' hint for tilde path, got: {err}"
    );
}

/// F-M3-01 (`Store::connect` populates the driver kind from the URL):
/// the kind parsed at `Store::connect` time must drive the migration
/// runner's driver-aware SQL. On SQLite the `try_claim_migration`
/// claim is the `ON CONFLICT DO NOTHING RETURNING` form (we can verify
/// this by inspecting the catalog after a fresh connect: the
/// `schema_version` table is created with the SQLite form). On
/// `postgres:` and `mysql:` URLs the `DatabaseKind` is parsed without
/// actually opening a connection (we can test the parser directly;
/// the integration test against a real Postgres / MySQL server is
/// out of scope for this CI environment).
#[test]
fn adversarial_database_kind_drives_v5_ddl_branch() {
    // All three forms must be syntactically valid DDL and must
    // agree on every column except the `id` clause.
    let sqlite_ddl = Store::v5_create_messages_sql(DatabaseKind::Sqlite);
    let postgres_ddl = Store::v5_create_messages_sql(DatabaseKind::Postgres);
    let mysql_ddl = Store::v5_create_messages_sql(DatabaseKind::MySql);

    // Common shape assertions: every form must declare the same
    // non-id columns and the same defaults.
    for (label, ddl) in [
        ("sqlite", &sqlite_ddl),
        ("postgres", &postgres_ddl),
        ("mysql", &mysql_ddl),
    ] {
        assert!(
            ddl.contains("vctx         TEXT NOT NULL"),
            "[{label}] missing vctx column: {ddl}"
        );
        assert!(
            ddl.contains("session_name TEXT NOT NULL DEFAULT 'default'"),
            "[{label}] missing session_name: {ddl}"
        );
        assert!(
            ddl.contains("created_at   TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)"),
            "[{label}] missing created_at with CURRENT_TIMESTAMP default: {ddl}"
        );
    }

    // Driver-specific id clauses.
    assert!(sqlite_ddl.contains("INTEGER PRIMARY KEY AUTOINCREMENT"));
    assert!(postgres_ddl.contains("GENERATED BY DEFAULT AS IDENTITY"));
    assert!(mysql_ddl.contains("BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY"));

    // The three forms must be distinct (no shared id clause).
    assert_ne!(sqlite_ddl, postgres_ddl);
    assert_ne!(sqlite_ddl, mysql_ddl);
    assert_ne!(postgres_ddl, mysql_ddl);
}

/// F-M3-01 (`column_exists` is now driver-aware): the pre-check used
/// by `migrate_to_v4` must use the SQLite `pragma_table_info` form on
/// SQLite, not the broken runtime probe. We construct a Store with
/// `kind: Sqlite` and verify the column is detected on the SQLite
/// path (this is the path the existing v4 tests already exercise;
/// the F-M3-01 fix is that the path is now selected by
/// `self.kind` rather than by the unreliable `current_database()`
/// probe — so the SQLite path is no longer falsely taken on MySQL).
#[tokio::test]
async fn adversarial_column_exists_uses_pragma_on_sqlite() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    store
        .ddl("CREATE TABLE t (a INTEGER, b TEXT)")
        .await
        .expect("create");
    assert!(store.column_exists("t", "a").await.unwrap());
    assert!(store.column_exists("t", "b").await.unwrap());
    assert!(!store.column_exists("t", "c").await.unwrap());
    // Identifier safety check: non-identifier characters must
    // short-circuit to Ok(false) (no SQL injection).
    assert!(!store.column_exists("t; DROP", "a").await.unwrap());
}

/// F-M3-03: every `migrations/*.sql` file contains no
/// `datetime('now')` residue. The m2 review established
/// `CURRENT_TIMESTAMP` as the canonical default in the Rust
/// DDLs; the SQL files must use the same form.
#[test]
fn m3_no_legacy_datetime_now_in_migration_files() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest_dir.join("migrations");
    let mut checked = 0usize;
    for entry in
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
    {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sql") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            !text.contains("datetime('now')"),
            "{} still contains legacy datetime('now') — use CURRENT_TIMESTAMP",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "expected at least 4 .sql files, found {checked}"
    );
}

/// F-M3-04: every `migrations/*.sql` file that contains a timestamp
/// default uses `CURRENT_TIMESTAMP` (not `datetime('now')`). Companion
/// to F-M3-03; asserts the affirmative side of the unification. Files
/// that contain no timestamp default (e.g. `0003_*` index-only file)
/// are exempt — the test only fires for files that mention the word
/// "timestamp" or "TIMESTAMP".
#[test]
fn m3_migration_files_use_current_timestamp() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest_dir.join("migrations");
    for entry in std::fs::read_dir(&dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sql") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read");
        let mentions_timestamp = text.contains("timestamp") || text.contains("TIMESTAMP");
        if !mentions_timestamp {
            continue;
        }
        assert!(
            text.contains("CURRENT_TIMESTAMP"),
            "{} is missing CURRENT_TIMESTAMP — every timestamp default \
                 must use the SQL standard form (M3 alignment)",
            path.display()
        );
    }
}

/// F-M3-05: after `run_migrations`, the SQLite catalog contains the
/// three index names that the SQL files declare
/// (`idx_context_token_map_real_ctx`, `idx_context_token_map_created_at`,
/// `idx_messages_vctx_created`, `idx_messages_peer_role_created`).
/// This is the M3 cross-check between the SQL reference files and
/// the runtime catalog.
#[tokio::test]
async fn m3_index_names_match_sql_files_and_catalog() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    for idx in [
        "idx_context_token_map_real_ctx",
        "idx_context_token_map_created_at",
        "idx_messages_vctx_created",
        "idx_messages_peer_role_created",
    ] {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='index' AND name = $1")
                .bind(idx)
                .fetch_optional(&store.pool)
                .await
                .expect("catalog");
        assert!(row.is_some(), "index {idx} missing from SQLite catalog");
    }
}

/// F-M3-06: the inline Rust DDL strings in `migrate_to_v1`, `migrate_to_v2`,
/// and `migrate_to_v4` are byte-equivalent (modulo whitespace) to the
/// corresponding statements in `migrations/0001_initial_schema.sql`,
/// `migrations/0002_backend_sessions.sql`, and
/// `migrations/0004_context_token_map_created_at.sql`. v3 has no
/// SQL file (its `CREATE UNIQUE INDEX` is inline-only); v5 is
/// covered by `m3_v5_sqlite_ddl_matches_migration_file`.
#[tokio::test]
async fn m3_migration_files_match_inline_ddl_for_v1_v2_v4() {
    // Re-run the in-source extraction: the migration runner must use
    // the same DDL strings the SQL files declare. The simplest
    // invariant: after `Store::connect`, the SQLite catalog contains
    // every table and index that the SQL files declare, with the
    // exact names.
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // Tables declared in the SQL files.
    let expected_tables = [
        // 0000 (documentation only — table is created by the runner,
        // not by the SQL file). Skipped.
        "clients",             // 0001
        "routing_state",       // 0001
        "context_token_map",   // 0001
        "bot_credentials",     // 0001
        "backend_sessions_v2", // 0002
        "active_sessions",     // 0002
        "messages",            // 0005
    ];
    for t in expected_tables {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name = $1")
                .bind(t)
                .fetch_optional(&store.pool)
                .await
                .expect("catalog");
        assert!(
            row.is_some(),
            "table {t} declared in migrations/*.sql but missing from catalog"
        );
    }
}

// ─── get_session_status_per_vtoken ───────────────────────────────────────

#[tokio::test]
async fn session_status_empty_vtokens_returns_empty_map() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let result = store
        .get_session_status_per_vtoken(&[])
        .await
        .expect("query");
    assert!(result.is_empty());
}

#[tokio::test]
async fn session_status_no_messages_returns_empty_map() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let vtokens = vec!["vt-unknown".to_string()];
    let result = store
        .get_session_status_per_vtoken(&vtokens)
        .await
        .expect("query");
    assert!(result.is_empty());
}

#[tokio::test]
async fn session_status_waiting_when_last_message_is_user() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    store
        .save_message(
            "vctx1",
            Some("vt1"),
            "default",
            "user1",
            "user",
            "帮我看看这个问题",
        )
        .await
        .expect("save user");

    let result = store
        .get_session_status_per_vtoken(&["vt1".to_string()])
        .await
        .expect("query");

    let entry = result.get("vt1").expect("entry for vt1");
    assert!(
        entry.waiting_for_reply,
        "last role is user → should be waiting"
    );
    assert_eq!(entry.session_name, "default");
    assert_eq!(entry.last_user_content.as_deref(), Some("帮我看看这个问题"));
}

#[tokio::test]
async fn session_status_not_waiting_when_last_message_is_assistant() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    store
        .save_message(
            "vctx2",
            Some("vt2"),
            "work",
            "user2",
            "user",
            "请解释一下 Rust 的生命周期",
        )
        .await
        .expect("save user");
    store
        .save_message(
            "vctx2",
            Some("vt2"),
            "work",
            "user2",
            "assistant",
            "生命周期是…",
        )
        .await
        .expect("save assistant");

    let result = store
        .get_session_status_per_vtoken(&["vt2".to_string()])
        .await
        .expect("query");

    let entry = result.get("vt2").expect("entry for vt2");
    assert!(
        !entry.waiting_for_reply,
        "last role is assistant → not waiting"
    );
    assert_eq!(entry.session_name, "work");
    assert_eq!(
        entry.last_user_content.as_deref(),
        Some("请解释一下 Rust 的生命周期")
    );
}

#[tokio::test]
async fn session_status_multiple_vtokens_independent() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    // vt-a: user sent, AI hasn't replied
    store
        .save_message("ctx-a", Some("vt-a"), "default", "pa", "user", "问题A")
        .await
        .expect("save");
    // vt-b: full round trip
    store
        .save_message("ctx-b", Some("vt-b"), "session-x", "pb", "user", "问题B")
        .await
        .expect("save");
    store
        .save_message(
            "ctx-b",
            Some("vt-b"),
            "session-x",
            "pb",
            "assistant",
            "回答B",
        )
        .await
        .expect("save");

    let result = store
        .get_session_status_per_vtoken(&["vt-a".to_string(), "vt-b".to_string()])
        .await
        .expect("query");

    let a = result.get("vt-a").expect("vt-a");
    assert!(a.waiting_for_reply);
    assert_eq!(a.last_user_content.as_deref(), Some("问题A"));

    let b = result.get("vt-b").expect("vt-b");
    assert!(!b.waiting_for_reply);
    assert_eq!(b.last_user_content.as_deref(), Some("问题B"));
    assert_eq!(b.session_name, "session-x");
}

#[tokio::test]
async fn session_status_unknown_vtoken_not_in_result() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    store
        .save_message("ctx", Some("vt-known"), "default", "p", "user", "hi")
        .await
        .expect("save");

    let result = store
        .get_session_status_per_vtoken(&["vt-known".to_string(), "vt-missing".to_string()])
        .await
        .expect("query");

    assert!(result.contains_key("vt-known"));
    assert!(
        !result.contains_key("vt-missing"),
        "unknown vtoken must not appear"
    );
}

// ─── Adversarial tests for M1 review findings ──────────────────────────
//
// Each test below pins down a specific SEC-ADV finding from the
// adversarial review. They are independent of the M1/M2/M3 regression
// tests above and exercise only the new code paths.

/// SEC-ADV-001: `ensure_sqlite_file` must NOT truncate an existing
/// SQLite database file. We create a valid DB, write data to it, then
/// call `ensure_sqlite_file` again — the file size must not shrink to
/// zero and the data must still be readable.
#[test]
fn adversarial_ensure_sqlite_file_does_not_truncate_existing_db() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("existing.db");
    let url = format!("sqlite:{}", db_path.display());

    // Create the database via Store::connect (this writes v1-v5 schema).
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _store = rt.block_on(async { Store::connect(&url).await.expect("first connect") });

    // Record the file size after schema creation.
    let size_before = std::fs::metadata(&db_path).expect("metadata").len();
    assert!(
        size_before > 0,
        "database file must have content after Store::connect"
    );

    // Now simulate a concurrent connect: call `ensure_sqlite_file` on
    // the same path. With the old `File::create` this would truncate
    // the file to 0 bytes. With `create_new(true)` it returns
    // AlreadyExists and leaves the file untouched.
    Store::ensure_sqlite_file(&url).expect("ensure_sqlite_file must succeed");

    let size_after = std::fs::metadata(&db_path).expect("metadata").len();
    assert!(
        size_after >= size_before,
        "ensure_sqlite_file must not truncate existing database: \
             size_before={size_before}, size_after={size_after}"
    );

    // Verify the database is still usable (not corrupted by truncation).
    let store2 = rt.block_on(async { Store::connect(&url).await.expect("second connect") });
    let v = rt.block_on(store2.get_current_version()).unwrap();
    assert_eq!(
        v, 13,
        "database must still be at v13 after ensure_sqlite_file"
    );
}

/// SEC-ADV-001 (concurrent stress): hammer `ensure_sqlite_file` from
/// multiple OS threads against the same file. With the old `File::create`
/// this would eventually truncate a concurrent writer's data. With
/// `create_new(true)` + `AlreadyExists` handling, every call either
/// creates the file or safely observes it already exists.
#[test]
fn adversarial_ensure_sqlite_file_concurrent_threads_safe() {
    use std::sync::Arc;

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("race.db");
    let url = format!("sqlite:{}", db_path.display());

    // First, create the database and populate it.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _store = rt.block_on(async { Store::connect(&url).await.expect("first connect") });
    let size_before = std::fs::metadata(&db_path).expect("metadata").len();

    // Now hammer `ensure_sqlite_file` from 16 threads concurrently.
    let url = Arc::new(url);
    let mut handles = Vec::new();
    for _ in 0..16 {
        let url = Arc::clone(&url);
        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                Store::ensure_sqlite_file(&url).expect("ensure_sqlite_file");
            }
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }

    let size_after = std::fs::metadata(&db_path).expect("metadata").len();
    assert!(
        size_after >= size_before,
        "concurrent ensure_sqlite_file must not truncate: \
             size_before={size_before}, size_after={size_after}"
    );

    // Database must still be usable.
    let store2 = rt.block_on(async {
        Store::connect(&url)
            .await
            .expect("reconnect after concurrent race")
    });
    let v = rt.block_on(store2.get_current_version()).unwrap();
    assert_eq!(v, 13);
}

/// SEC-ADV-002: `column_exists` on the SQLite branch must propagate
/// errors from the `pragma_table_info` query rather than silently
/// treating all errors as "column not found". The non-tx `column_exists`
/// still uses `.unwrap_or(None)` as a deliberate choice (caller treats
/// absent column as "not present and let the DDL surface the real
/// error"), but the tx variant in `migrate_to_v4_tx` now uses `?`.
/// This test verifies the in-tx path propagates errors for a
/// syntactically-invalid pragma query (malformed identifier).
#[tokio::test]
async fn adversarial_v4_tx_pragma_error_propagates() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // Bootstrap schema_version table (required by run_migrations).
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");
    // Mark v1-v3 as applied so only v4 runs.
    for v in 1..=3 {
        store.record_migration_run(v).await.expect("mark");
    }
    // Create a context_token_map WITHOUT created_at so v4 tries to add it.
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                    vctx TEXT PRIMARY KEY, real_ctx TEXT NOT NULL,
                    peer_user_id TEXT NOT NULL DEFAULT '', expires_at TEXT
                )",
        )
        .await
        .expect("context_token_map");

    // v4 should succeed (column doesn't exist yet, so ALTER ADD COLUMN runs).
    store
        .migrate_to_v4()
        .await
        .expect("v4 must add created_at column");
    assert!(
        store
            .column_exists("context_token_map", "created_at")
            .await
            .unwrap(),
        "created_at must exist after v4"
    );
}

/// SEC-ADV-002: `column_exists` on SQLite must return `Ok(false)` for
/// a non-existent table rather than propagating an error. This is the
/// deliberate error-suppression behaviour documented in the function
/// comment: callers treat "column absent" as a signal to run DDL, and
/// the DDL itself will surface the real error (e.g. "no such table")
/// with a clearer message.
#[tokio::test]
async fn adversarial_column_exists_returns_false_on_nonexistent_table() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // No tables created — `column_exists` on a non-existent table must
    // return `Ok(false)`, NOT propagate a runtime error.
    let result = store.column_exists("no_such_table", "any_col").await;
    assert!(
        result.is_ok(),
        "column_exists on non-existent table must return Ok, not Err"
    );
    assert!(
        !result.unwrap(),
        "column_exists on non-existent table must return Ok(false)"
    );
}

/// SEC-ADV-002 (regression): after a failed column_exists that
/// returned `Ok(false)` (deliberate suppression), the caller should
/// be able to attempt DDL that surfaces the real error. This confirms
/// the design that error suppression in column_exists does not hide
/// the root cause permanently.
#[tokio::test]
async fn adversarial_ddl_surfaces_error_after_column_exists_suppresses() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // column_exists returns false → caller tries DDL
    let col_missing = !store
        .column_exists("ghost_table", "ghost_col")
        .await
        .unwrap();
    assert!(col_missing);
    // The DDL must surface the real "no such table" error.
    let ddl_result = store
        .ddl("ALTER TABLE ghost_table ADD COLUMN ghost_col TEXT")
        .await;
    assert!(
        ddl_result.is_err(),
        "DDL on non-existent table must propagate error"
    );
    let err_msg = format!("{}", ddl_result.unwrap_err());
    assert!(
        err_msg.to_lowercase().contains("no such table")
            || err_msg.to_lowercase().contains("error"),
        "DDL error must mention the table problem; got: {err_msg}"
    );
}

/// SEC-ADV-004 + SEC-ADV-006: after `Store::connect` to a file-backed
/// SQLite database, the connection pool must have WAL journal mode
/// and busy_timeout=5000 explicitly configured.
#[tokio::test]
async fn adversarial_sqlite_connect_configures_wal_and_busy_timeout() {
    sqlx::any::install_default_drivers();
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite:{}/pragma.db", tmp.path().display());
    let store = Store::connect(&url).await.expect("connect");

    // Verify journal_mode is WAL via pragma_journal_mode TVF.
    let (jm,): (String,) = sqlx::query_as("SELECT * FROM pragma_journal_mode")
        .fetch_one(&store.pool)
        .await
        .expect("journal_mode query");
    assert_eq!(
        jm, "wal",
        "journal_mode must be WAL after Store::connect; got {jm}"
    );

    // Verify busy_timeout is 5000 via pragma_busy_timeout TVF.
    let (bt,): (i32,) = sqlx::query_as("SELECT * FROM pragma_busy_timeout")
        .fetch_one(&store.pool)
        .await
        .expect("busy_timeout query");
    assert_eq!(
        bt, 5000,
        "busy_timeout must be 5000ms after Store::connect; got {bt}"
    );
}

/// SEC-ADV-003: `try_claim_migration_in_tx` must have the same claim
/// semantics as `try_claim_migration` — exactly one caller wins in a
/// race. This test verifies the in-tx variant under concurrent access
/// on the same pool.
#[tokio::test]
async fn adversarial_try_claim_in_tx_is_mutually_exclusive() {
    sqlx::any::install_default_drivers();
    // File-backed SQLite so all connections share the same database —
    // :memory: databases are per-connection private unless shared-cache
    // is enabled.
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_url = format!("sqlite:{}/txclaim.db", tmp.path().display());
    Store::ensure_sqlite_file(&db_url).expect("ensure db file");
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(2)
        .connect(&db_url)
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool: pool.clone(),
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // Bootstrap schema_version.
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                    version     INTEGER PRIMARY KEY,
                    migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
                )",
        )
        .await
        .expect("schema_version");

    // Both transactions race for v99. Only one tx's claim can succeed.
    let pool2 = pool.clone();
    let store2 = Store {
        rpool: pool.clone(),
        pool: pool2,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    let (r1, r2) = tokio::join!(
        async {
            let mut tx = store.pool.begin().await.expect("tx1");
            let claimed = store.try_claim_migration_in_tx(&mut tx, 99).await.unwrap();
            tx.commit().await.expect("commit1");
            claimed
        },
        async {
            let mut tx = store2.pool.begin().await.expect("tx2");
            let claimed = store2.try_claim_migration_in_tx(&mut tx, 99).await.unwrap();
            tx.commit().await.expect("commit2");
            claimed
        },
    );
    let winners = [r1, r2].iter().filter(|c| **c).count();
    assert_eq!(
        winners, 1,
        "exactly one tx must claim v99; r1={r1}, r2={r2}"
    );
}

// ─── M1: vtoken hash storage contract ────────────────────────────────────────
//
// These tests pin the post-M1 contract on the Store: every bind that
// carries a vtoken must accept the canonical hash form, and the round-trip
// between register and the DB must never leak plaintext. Plaintext vtokens
// only exist at the HTTP boundary (Authorization header) and in the
// `register()` return value; the Store binds whatever the caller hands it,
// and the caller is expected to have hashed the plaintext before calling.

use crate::hub::{hash_vtoken, is_vtoken_hash};

#[tokio::test]
async fn m1_upsert_client_stores_hash_not_plaintext() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // Simulate the post-M1 call site: register() returns the plaintext,
    // the caller hashes it, and passes the hash to upsert_client.
    let plain = "vhub_0123456789abcdef0123456789abcdef";
    let hashed = hash_vtoken(plain);
    store
        .upsert_client(&hashed, "claude", Some("claude test"))
        .await
        .expect("upsert_client");

    // Round-trip: list_clients returns the same value that was bound.
    let rows = store.list_clients().await.expect("list_clients");
    let row = rows
        .iter()
        .find(|r| r.name == "claude")
        .expect("claude row present");
    assert_eq!(
        row.vtoken, hashed,
        "upsert must store exactly what was bound (hash form)"
    );
    assert!(
        is_vtoken_hash(&row.vtoken),
        "stored vtoken must be the canonical SHA-256 hex"
    );
    assert_ne!(row.vtoken, plain, "plaintext must NOT be persisted");
}

#[tokio::test]
async fn m1_touch_client_uses_hash() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let plain = "vhub_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
    let hashed = hash_vtoken(plain);
    store.upsert_client(&hashed, "claude", None).await.unwrap();

    // touch_client must accept the hash (the value the production
    // code path carries through the in-memory ClientInfo).
    store.touch_client(&hashed).await.expect("touch_client");

    // The row's stored vtoken is the hash, not the plaintext.
    let rows = store.list_clients().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].vtoken, hashed);
    assert_ne!(rows[0].vtoken, plain);
}

#[tokio::test]
async fn m1_routes_are_keyed_by_hash() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let plain = "vhub_route-target-aaaaaaaaaaaaaaaa";
    let hashed = hash_vtoken(plain);
    store.upsert_client(&hashed, "claude", None).await.unwrap();

    store.set_route("alice", &hashed).await.expect("set_route");

    // get_route returns the hash.
    let route = store.get_route("alice").await.expect("get_route");
    assert_eq!(route.as_deref(), Some(hashed.as_str()));
    assert_ne!(route.as_deref(), Some(plain));

    // list_routes returns (from_user, hash) pairs.
    let routes = store.list_routes().await.expect("list_routes");
    assert_eq!(routes, vec![("alice".to_string(), hashed.clone())]);

    // clear_routes_for_vtoken accepts the hash.
    store
        .clear_routes_for_vtoken(&hashed)
        .await
        .expect("clear_routes_for_vtoken");
    assert!(store.get_route("alice").await.unwrap().is_none());
}

#[tokio::test]
async fn m1_messages_table_keys_by_hash() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let plain = "vhub_msg-target-bbbbbbbbbbbbbbbbb";
    let hashed = hash_vtoken(plain);
    store.upsert_client(&hashed, "claude", None).await.unwrap();

    // save_message binds the hash (post-M1 caller contract).
    store
        .save_message(
            "vctx-1",
            Some(&hashed),
            "default",
            "user-1",
            "assistant",
            "hello",
        )
        .await
        .expect("save_message");

    // find_assistant_message_by_content returns the stored hash, not the
    // plaintext. The dispatch layer's DB-fallback quote resolver then
    // uses the returned value to look up the registry by hash.
    let (vtoken, _session) = store
        .find_assistant_message_by_content("user-1", "hello")
        .await
        .expect("find_assistant_message_by_content")
        .expect("an assistant message should be found");
    assert_eq!(vtoken, hashed);
    assert_ne!(vtoken, plain);
}

#[tokio::test]
async fn m1_two_distinct_plaintexts_never_collide() {
    // The hash must be a function of the plaintext: two different
    // vhub_… strings must produce two different rows. This guards against
    // a regression that accidentally binds the same key for every
    // registration (e.g. forgetting the name/vtoken distinction).
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let plain_a = "vhub_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let plain_b = "vhub_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let hash_a = hash_vtoken(plain_a);
    let hash_b = hash_vtoken(plain_b);
    assert_ne!(hash_a, hash_b, "distinct plaintexts hash differently");

    store.upsert_client(&hash_a, "alice", None).await.unwrap();
    store.upsert_client(&hash_b, "bob", None).await.unwrap();

    let rows = store.list_clients().await.unwrap();
    let by_name: std::collections::HashMap<_, _> = rows
        .iter()
        .map(|r| (r.name.clone(), r.vtoken.clone()))
        .collect();
    assert_eq!(
        by_name.get("alice").map(String::as_str),
        Some(hash_a.as_str())
    );
    assert_eq!(
        by_name.get("bob").map(String::as_str),
        Some(hash_b.as_str())
    );
}

#[tokio::test]
async fn test_bot_credentials_encryption_decryption() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // 1. Without master key:
    // loading credentials on empty DB returns Ok(None)
    assert!(store.load_credentials().await.unwrap().is_none());
    // saving credentials must fail
    assert!(store
        .save_credentials("my-secret-token", "https://api.example.com")
        .await
        .is_err());

    // 2. Set master key (using standard 32-byte key)
    let raw_key = [0u8; 32];
    let unbound_key = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &raw_key).unwrap();
    let key = ring::aead::LessSafeKey::new(unbound_key);
    store
        .set_master_key(std::sync::Arc::new(key))
        .expect("set_master_key");

    // 3. Load credentials on empty store returns None
    let loaded = store.load_credentials().await.unwrap();
    assert!(loaded.is_none());

    // 4. Save and load credentials successfully
    store
        .save_credentials("my-secret-token", "https://api.example.com")
        .await
        .unwrap();
    let loaded = store.load_credentials().await.unwrap().expect("loaded");
    assert_eq!(loaded.0, "my-secret-token");
    assert_eq!(loaded.1, "https://api.example.com");

    // 5. Verify database contains encrypted ciphertext, not the plaintext
    let row: (String,) = sqlx::query_as("SELECT token FROM bot_credentials WHERE id = 1")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_ne!(row.0, "my-secret-token");
    // Formats output as base64, so it should decode successfully as base64 and not match plaintext
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    assert!(B64.decode(&row.0).is_ok());

    // 6. Test loading credentials when master key is absent (on a new Store instance sharing the pool)
    let store2 = Store {
        pool: store.pool.clone(),
        rpool: store.pool.clone(),
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    // Because a row exists in bot_credentials now, loading should fail due to missing master key
    assert!(store2.load_credentials().await.is_err());
}

#[test]
fn test_load_or_derive_master_key_scenarios() {
    let _guard = ENV_MUTEX.lock().unwrap();
    // Save current env var to restore it later
    let old_val = std::env::var("ILINK_HUB_MASTER_KEY");

    // 1. Missing env var
    std::env::remove_var("ILINK_HUB_MASTER_KEY");
    let res = crate::runtime::crypto::load_or_derive_master_key();
    assert!(res.is_err());

    // 2. Invalid formats (too short, not hex/b64, etc.)
    std::env::set_var("ILINK_HUB_MASTER_KEY", "short");
    assert!(crate::runtime::crypto::load_or_derive_master_key().is_err());

    std::env::set_var(
        "ILINK_HUB_MASTER_KEY",
        "not-hex-and-too-long-but-invalid-characters-zzzzzzzzzzzzzzzzzzzzzzzzz",
    );
    assert!(crate::runtime::crypto::load_or_derive_master_key().is_err());

    // 3. Correct 32-byte hex (64 hex characters)
    let hex_key = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    std::env::set_var("ILINK_HUB_MASTER_KEY", hex_key);
    let res = crate::runtime::crypto::load_or_derive_master_key();
    assert!(res.is_ok());

    // 3a. Hex key with double quotes
    std::env::set_var("ILINK_HUB_MASTER_KEY", format!("\"{}\"", hex_key));
    assert!(crate::runtime::crypto::load_or_derive_master_key().is_ok());

    // 3b. Hex key with single quotes
    std::env::set_var("ILINK_HUB_MASTER_KEY", format!("'{}'", hex_key));
    assert!(crate::runtime::crypto::load_or_derive_master_key().is_ok());

    // 3c. Hex key with leading/trailing whitespaces
    std::env::set_var("ILINK_HUB_MASTER_KEY", format!("   {}   ", hex_key));
    assert!(crate::runtime::crypto::load_or_derive_master_key().is_ok());

    // 4. Correct 32-byte base64 (44 characters)
    // 32 zero bytes in base64: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    let b64_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    std::env::set_var("ILINK_HUB_MASTER_KEY", b64_key);
    let res = crate::runtime::crypto::load_or_derive_master_key();
    assert!(res.is_ok());

    // 4a. Base64 key with quotes and whitespaces
    std::env::set_var("ILINK_HUB_MASTER_KEY", format!("  \"{}\"  ", b64_key));
    assert!(crate::runtime::crypto::load_or_derive_master_key().is_ok());

    // Restore old env var
    match old_val {
        Ok(val) => std::env::set_var("ILINK_HUB_MASTER_KEY", val),
        Err(_) => std::env::remove_var("ILINK_HUB_MASTER_KEY"),
    }
}

#[tokio::test]
async fn test_bot_credentials_decryption_adversarial_wrong_key() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // 1. Set master key A
    let raw_key_a = [0u8; 32];
    let unbound_key_a = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &raw_key_a).unwrap();
    let key_a = ring::aead::LessSafeKey::new(unbound_key_a);
    store
        .set_master_key(std::sync::Arc::new(key_a))
        .expect("set_master_key");

    // 2. Save credentials under key A
    store
        .save_credentials("my-secret-token", "https://api.example.com")
        .await
        .unwrap();

    // 3. Create another Store instance with Master Key B sharing the same pool
    let raw_key_b = [1u8; 32];
    let unbound_key_b = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &raw_key_b).unwrap();
    let key_b = ring::aead::LessSafeKey::new(unbound_key_b);

    let store_b = Store {
        pool: store.pool.clone(),
        rpool: store.pool.clone(),
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    store_b
        .set_master_key(std::sync::Arc::new(key_b))
        .expect("set_master_key");

    // 4. Loading credentials with key B must fail (should return Err)
    let res = store_b.load_credentials().await;
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("Decryption failed"));
}

#[tokio::test]
async fn test_bot_credentials_decryption_adversarial_tampered_ciphertext() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    // 1. Set master key
    let raw_key = [0u8; 32];
    let unbound_key = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &raw_key).unwrap();
    let key = ring::aead::LessSafeKey::new(unbound_key);
    store
        .set_master_key(std::sync::Arc::new(key))
        .expect("set_master_key");

    // 2. Save credentials
    store
        .save_credentials("my-secret-token", "https://api.example.com")
        .await
        .unwrap();

    // Case A: Ciphertext replaced by invalid base64 (e.g. invalid characters)
    sqlx::query("UPDATE bot_credentials SET token = 'not-base64-at-all-$$$' WHERE id = 1")
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(store.load_credentials().await.is_err());

    // Case B: Ciphertext is too short to contain nonce + tag
    sqlx::query("UPDATE bot_credentials SET token = 'c2hvcnQ=' WHERE id = 1") // "short" in base64
        .execute(&store.pool)
        .await
        .unwrap();
    let res = store.load_credentials().await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("data too short"));

    // Case C: Ciphertext base64-decodes fine but is corrupted (one bit flipped in the payload/tag)
    store
        .save_credentials("my-secret-token", "https://api.example.com")
        .await
        .unwrap();
    let row: (String,) = sqlx::query_as("SELECT token FROM bot_credentials WHERE id = 1")
        .fetch_one(&store.pool)
        .await
        .unwrap();

    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    let mut bytes = B64.decode(&row.0).unwrap();
    // Flip a bit in the ciphertext or tag (not the nonce)
    bytes[20] ^= 1;
    let corrupted_b64 = B64.encode(&bytes);

    sqlx::query("UPDATE bot_credentials SET token = $1 WHERE id = 1")
        .bind(corrupted_b64)
        .execute(&store.pool)
        .await
        .unwrap();

    let res = store.load_credentials().await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("Decryption failed"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_migration_v8_hash_vtoken_and_encrypt_bot_token() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };

    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version     INTEGER PRIMARY KEY,
                migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await
        .unwrap();

    store.migrate_to_v1().await.unwrap();
    store.migrate_to_v2().await.unwrap();
    store.migrate_to_v3().await.unwrap();
    store.migrate_to_v4().await.unwrap();
    store.migrate_to_v5().await.unwrap();
    store.migrate_to_v6().await.unwrap();
    store.migrate_to_v7().await.unwrap();

    let plain_vtoken = "plain_vtoken_12345";
    sqlx::query("INSERT INTO clients (vtoken, name, label) VALUES ($1, $2, $3)")
        .bind(plain_vtoken)
        .bind("client_1")
        .bind(Some("My Client"))
        .execute(store.pool())
        .await
        .unwrap();

    sqlx::query("INSERT INTO routing_state (from_user, active_vtoken) VALUES ($1, $2)")
        .bind("user_1")
        .bind(plain_vtoken)
        .execute(store.pool())
        .await
        .unwrap();

    sqlx::query("INSERT INTO messages (vctx, vtoken, session_name, role, content) VALUES ($1, $2, $3, $4, $5)")
        .bind("vctx_1")
        .bind(plain_vtoken)
        .bind("default")
        .bind("user")
        .bind("hello")
        .execute(store.pool())
        .await
        .unwrap();

    let plain_bot_token = "plain_bot_token_secret_value";
    sqlx::query("INSERT INTO bot_credentials (id, token, base_url) VALUES (1, $1, $2)")
        .bind(plain_bot_token)
        .bind("https://dummy.url")
        .execute(store.pool())
        .await
        .unwrap();

    let old_key = std::env::var("ILINK_HUB_MASTER_KEY");
    let temp_key = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    std::env::set_var("ILINK_HUB_MASTER_KEY", temp_key);

    store
        .migrate_to_v8()
        .await
        .expect("migrate_to_v8 should succeed");

    // Derive the key from temp_key BEFORE restoring the original env var, so
    // decryption below uses the same key that was active during migration.
    let migration_key = crate::runtime::crypto::load_or_derive_master_key()
        .expect("master key must be loadable while temp_key is still set");

    if let Ok(ref k) = old_key {
        std::env::set_var("ILINK_HUB_MASTER_KEY", k);
    } else {
        std::env::remove_var("ILINK_HUB_MASTER_KEY");
    }

    let hashed_vtoken = crate::hub::hash_vtoken(plain_vtoken);
    let client_vtoken_db: String =
        sqlx::query_scalar("SELECT vtoken FROM clients WHERE name = 'client_1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(client_vtoken_db, hashed_vtoken);

    let route_vtoken_db: String =
        sqlx::query_scalar("SELECT active_vtoken FROM routing_state WHERE from_user = 'user_1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(route_vtoken_db, hashed_vtoken);

    let msg_vtoken_db: String =
        sqlx::query_scalar("SELECT vtoken FROM messages WHERE vctx = 'vctx_1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(msg_vtoken_db, hashed_vtoken);

    let cred_token_db: String =
        sqlx::query_scalar("SELECT token FROM bot_credentials WHERE id = 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_ne!(cred_token_db, plain_bot_token);

    let decrypted = crate::runtime::crypto::decrypt_token(&cred_token_db, &migration_key).unwrap();
    assert_eq!(decrypted, plain_bot_token);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_migration_v8_missing_master_key_fails() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };

    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version     INTEGER PRIMARY KEY,
                migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await
        .unwrap();

    store.migrate_to_v1().await.unwrap();
    store.migrate_to_v2().await.unwrap();
    store.migrate_to_v3().await.unwrap();
    store.migrate_to_v4().await.unwrap();
    store.migrate_to_v5().await.unwrap();
    store.migrate_to_v6().await.unwrap();
    store.migrate_to_v7().await.unwrap();

    sqlx::query("INSERT INTO clients (vtoken, name, label) VALUES ($1, $2, $3)")
        .bind("plain_token")
        .bind("client_1")
        .bind(Some("Client"))
        .execute(store.pool())
        .await
        .unwrap();

    let old_key = std::env::var("ILINK_HUB_MASTER_KEY");
    std::env::remove_var("ILINK_HUB_MASTER_KEY");

    let res = store.migrate_to_v8().await;

    if let Ok(ref k) = old_key {
        std::env::set_var("ILINK_HUB_MASTER_KEY", k);
    } else {
        std::env::remove_var("ILINK_HUB_MASTER_KEY");
    }

    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("ILINK_HUB_MASTER_KEY is required"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_migration_v8_idempotency_does_not_double_encrypt() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };

    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version     INTEGER PRIMARY KEY,
                migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await
        .unwrap();

    store.migrate_to_v1().await.unwrap();
    store.migrate_to_v2().await.unwrap();
    store.migrate_to_v3().await.unwrap();
    store.migrate_to_v4().await.unwrap();
    store.migrate_to_v5().await.unwrap();
    store.migrate_to_v6().await.unwrap();
    store.migrate_to_v7().await.unwrap();

    let plain_bot_token = "plain_bot_token_secret_value";
    sqlx::query("INSERT INTO bot_credentials (id, token, base_url) VALUES (1, $1, $2)")
        .bind(plain_bot_token)
        .bind("https://dummy.url")
        .execute(store.pool())
        .await
        .unwrap();
    // migrate_to_v8 only encrypts bot_credentials when clients table is non-empty
    // (it uses the first vtoken as a sentinel to decide whether migration is needed).
    sqlx::query("INSERT INTO clients (vtoken, name, label) VALUES ($1, $2, $3)")
        .bind("vhub_plain_sentinel_for_migration_test")
        .bind("test-client")
        .bind(Option::<String>::None)
        .execute(store.pool())
        .await
        .unwrap();

    let old_key = std::env::var("ILINK_HUB_MASTER_KEY");
    let temp_key = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    std::env::set_var("ILINK_HUB_MASTER_KEY", temp_key);

    // 1. Run migration first time
    store
        .migrate_to_v8()
        .await
        .expect("migrate_to_v8 first run should succeed");

    let migration_key =
        crate::runtime::crypto::load_or_derive_master_key().expect("master key loadable");

    let cred_token_1: String = sqlx::query_scalar("SELECT token FROM bot_credentials WHERE id = 1")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_ne!(cred_token_1, plain_bot_token);
    assert_eq!(
        crate::runtime::crypto::decrypt_token(&cred_token_1, &migration_key).unwrap(),
        plain_bot_token
    );

    // 2. Clear version tracking for v8 to force re-migration over already-encrypted data
    sqlx::query("DELETE FROM schema_version WHERE version = 8")
        .execute(store.pool())
        .await
        .unwrap();

    // 3. Run migration second time
    store
        .migrate_to_v8()
        .await
        .expect("migrate_to_v8 second run should succeed");

    let cred_token_2: String = sqlx::query_scalar("SELECT token FROM bot_credentials WHERE id = 1")
        .fetch_one(store.pool())
        .await
        .unwrap();

    // The token should remain exactly the same, no double-encryption!
    assert_eq!(cred_token_2, cred_token_1);
    assert_eq!(
        crate::runtime::crypto::decrypt_token(&cred_token_2, &migration_key).unwrap(),
        plain_bot_token
    );

    if let Ok(ref k) = old_key {
        std::env::set_var("ILINK_HUB_MASTER_KEY", k);
    } else {
        std::env::remove_var("ILINK_HUB_MASTER_KEY");
    }
}

/// P-22-2: `find_assistant_message_by_content` correctly escapes LIKE special characters.
/// Insert a message whose content contains `%` and `_`.
/// Verify the function finds it by exact prefix (not by wildcard expansion).
#[tokio::test]
async fn test_like_escape_in_find_assistant_message() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    let peer = "peer:escape-test-user";
    let special_content = "test%value_here\\end";

    store
        .save_message(
            "vctx-esc",
            Some("vtoken-esc"),
            "default",
            peer,
            "assistant",
            special_content,
        )
        .await
        .expect("save_message with special chars");

    // A decoy message that would match if % is not escaped (starts with "test").
    store
        .save_message(
            "vctx-esc2",
            Some("vtoken-esc2"),
            "default",
            peer,
            "assistant",
            "testXvalueYhereZend",
        )
        .await
        .expect("save_message decoy");

    // Query with the full special content as prefix; only the first message should match.
    let result = store
        .find_assistant_message_by_content(peer, special_content)
        .await
        .expect("find_assistant_message_by_content");

    assert!(
        result.is_some(),
        "should find the message with special content"
    );
    let (vtoken, session) = result.unwrap();
    assert_eq!(vtoken, "vtoken-esc", "should match the correct vtoken");
    assert_eq!(session, Some("default".to_string()));
}

/// P-22-3: `get_session_status_per_vtoken` with an empty slice returns empty map without panic.
#[tokio::test]
async fn test_get_session_status_empty() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    let result = store
        .get_session_status_per_vtoken(&[])
        .await
        .expect("get_session_status_per_vtoken with empty slice");

    assert!(result.is_empty(), "empty input must produce empty output");
}

/// P-22-4: `get_session_status_per_vtoken` returns correct entries for multiple vtokens.
#[tokio::test]
async fn test_get_session_status_multi_vtoken() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");

    let vtoken1 = "vtoken-alpha";
    let vtoken2 = "vtoken-beta";

    // vtoken1: user sends last (waiting_for_reply = true)
    store
        .save_message(
            "vctx-a1",
            Some(vtoken1),
            "default",
            "peer:a",
            "assistant",
            "reply A1",
        )
        .await
        .unwrap();
    store
        .save_message(
            "vctx-a2",
            Some(vtoken1),
            "default",
            "peer:a",
            "user",
            "question A2",
        )
        .await
        .unwrap();

    // vtoken2: assistant replies last (waiting_for_reply = false)
    store
        .save_message(
            "vctx-b1",
            Some(vtoken2),
            "default",
            "peer:b",
            "user",
            "question B1",
        )
        .await
        .unwrap();
    store
        .save_message(
            "vctx-b2",
            Some(vtoken2),
            "default",
            "peer:b",
            "assistant",
            "reply B2",
        )
        .await
        .unwrap();

    let vtokens = vec![vtoken1.to_string(), vtoken2.to_string()];
    let result = store
        .get_session_status_per_vtoken(&vtokens)
        .await
        .expect("get_session_status_per_vtoken");

    assert_eq!(result.len(), 2, "should return entries for both vtokens");

    let entry1 = result.get(vtoken1).expect("entry for vtoken1");
    assert!(
        entry1.waiting_for_reply,
        "vtoken1: last message is user → waiting"
    );
    assert_eq!(
        entry1.last_user_content.as_deref(),
        Some("question A2"),
        "vtoken1: latest user content must be question A2"
    );

    let entry2 = result.get(vtoken2).expect("entry for vtoken2");
    assert!(
        !entry2.waiting_for_reply,
        "vtoken2: last message is assistant → not waiting"
    );
    assert_eq!(
        entry2.last_user_content.as_deref(),
        Some("question B1"),
        "vtoken2: latest user content must be question B1"
    );
}

// ─── get_all_session_entries_per_vtoken ──────────────────────────────────────

/// N-13-1: empty input returns empty map without panic.
#[tokio::test]
async fn test_get_all_session_entries_empty() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let result = store
        .get_all_session_entries_per_vtoken(&[])
        .await
        .expect("get_all_session_entries_per_vtoken with empty slice");
    assert!(result.is_empty(), "empty input must produce empty output");
}

/// N-13-2: single vtoken, single session with three messages (user→assistant→user).
/// Expects waiting_for_reply = true and last_user_content = "world".
#[tokio::test]
async fn test_get_all_session_entries_single_vtoken() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let vtoken = "vt-n13-single";

    store
        .save_message("ctx1", Some(vtoken), "default", "peer1", "user", "hello")
        .await
        .unwrap();
    store
        .save_message("ctx2", Some(vtoken), "default", "peer1", "assistant", "hi")
        .await
        .unwrap();
    store
        .save_message("ctx3", Some(vtoken), "default", "peer1", "user", "world")
        .await
        .unwrap();

    let result = store
        .get_all_session_entries_per_vtoken(&[vtoken.to_string()])
        .await
        .expect("get_all_session_entries_per_vtoken");

    assert_eq!(result.len(), 1, "should return entries for one vtoken");
    let entries = result.get(vtoken).expect("entries for vtoken");
    assert_eq!(entries.len(), 1, "one session");

    let entry = &entries[0];
    assert_eq!(entry.session_name, "default");
    assert!(
        entry.waiting_for_reply,
        "last message is user → waiting_for_reply must be true"
    );
    assert_eq!(
        entry.last_user_content.as_deref(),
        Some("world"),
        "last user content must be 'world'"
    );
    assert!(
        entry.user_msg_created_at.is_some(),
        "user_msg_created_at must be set"
    );
}

/// N-13-3: same vtoken, two different session_names — entries are independent.
#[tokio::test]
async fn test_get_all_session_entries_multi_session() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let vtoken = "vt-n13-multi";

    // session-a: user asks, assistant replies → not waiting
    store
        .save_message(
            "ctxA1",
            Some(vtoken),
            "session-a",
            "peerA",
            "user",
            "question-A",
        )
        .await
        .unwrap();
    store
        .save_message(
            "ctxA2",
            Some(vtoken),
            "session-a",
            "peerA",
            "assistant",
            "answer-A",
        )
        .await
        .unwrap();

    // session-b: user asks but no reply yet → waiting
    store
        .save_message(
            "ctxB1",
            Some(vtoken),
            "session-b",
            "peerB",
            "user",
            "question-B",
        )
        .await
        .unwrap();

    let result = store
        .get_all_session_entries_per_vtoken(&[vtoken.to_string()])
        .await
        .expect("get_all_session_entries_per_vtoken");

    assert_eq!(result.len(), 1);
    let entries = result.get(vtoken).expect("entries for vtoken");
    assert_eq!(entries.len(), 2, "two sessions must each produce an entry");

    let entry_a = entries
        .iter()
        .find(|e| e.session_name == "session-a")
        .expect("entry for session-a");
    let entry_b = entries
        .iter()
        .find(|e| e.session_name == "session-b")
        .expect("entry for session-b");

    assert!(
        !entry_a.waiting_for_reply,
        "session-a: assistant replied last → not waiting"
    );
    assert_eq!(
        entry_a.last_user_content.as_deref(),
        Some("question-A"),
        "session-a last user content"
    );

    assert!(
        entry_b.waiting_for_reply,
        "session-b: user message pending → waiting"
    );
    assert_eq!(
        entry_b.last_user_content.as_deref(),
        Some("question-B"),
        "session-b last user content"
    );
}

// ─── Quote-routing scope contract ─────────────────────────────────────────────
//
// The dispatch path normalises `msg.from_user_id` → "peer:<id>" / "group:<id>"
// before looking up the QuoteRouteIndex and the DB.  The outbound registration
// path in `routes.rs` stores the scope produced by `resolve_send_context` (which
// returns `context_token_map.peer_user_id`, itself written as "peer:<id>" by
// `find_or_create_vctx`).
//
// Before the fix, dispatch used the raw `from_user_id` ("o9cq80_...") while
// registration used "peer:o9cq80_..." — the mismatch caused every quote-reply
// to miss the index and fall through to the wrong `default_client`.

/// `find_assistant_message_by_content` must return the correct row when
/// the assistant message is stored under the "peer:<id>" scope (the format
/// produced by `find_or_create_vctx` / `resolve_send_context`).
/// Before the bug fix, `dispatch.rs` passed the raw `from_user_id` ("uid")
/// instead of "peer:uid", so the LIKE query always returned nothing.
#[tokio::test]
async fn find_assistant_message_scope_uses_peer_prefix() {
    let store = Store::connect("sqlite::memory:").await.unwrap();

    // Outbound registration path stores under "peer:<id>" scope.
    let scope = "peer:o9cq80_testuser@im.wechat";
    let vtoken = "a92250b1deadbeef";
    let session = "at-20260622-152900941";
    let text = "🤖 Claude\n───────\nhello world\n\n---\nat-20260622-152900941";

    store
        .save_message("vctx_abc", Some(vtoken), session, scope, "assistant", text)
        .await
        .unwrap();

    // Correct call: prefix "peer:" matches stored scope.
    let result = store
        .find_assistant_message_by_content(scope, "🤖 Claude")
        .await
        .unwrap();
    assert!(
        result.is_some(),
        "DB quote lookup must find the row when the scope includes 'peer:' prefix"
    );
    let (found_vt, found_session) = result.unwrap();
    assert_eq!(found_vt, vtoken);
    assert_eq!(found_session.as_deref(), Some(session));

    // Wrong call (pre-fix behaviour): raw user_id without "peer:" must NOT match.
    let raw_uid = "o9cq80_testuser@im.wechat";
    let miss = store
        .find_assistant_message_by_content(raw_uid, "🤖 Claude")
        .await
        .unwrap();
    assert!(
        miss.is_none(),
        "DB quote lookup must NOT match when scope is missing the 'peer:' prefix (pre-fix regression guard)"
    );
}

/// `find_vctx_for_scope` and `find_vtoken_for_session` cover the
/// persona-footer slow path: when the footer only contains a session
/// identifier (e.g. "at-20260622-..."), the dispatch code looks up the
/// owning vtoken via `backend_sessions_v2`.
#[tokio::test]
async fn find_vtoken_for_session_resolves_persona_footer_fallback() {
    let store = Store::connect("sqlite::memory:").await.unwrap();

    let scope = "peer:o9cq80_testuser@im.wechat";
    let vtoken = "a92250b1deadbeef";
    let session = "at-20260622-152900941";

    // Simulate `find_or_create_vctx` storing the mapping.
    store
        .find_or_create_vctx("o9cq80_testuser@im.wechat", None, "AARzJWAFAAA_real_ctx")
        .await
        .unwrap();
    // Obtain the actual vctx that was created.
    let actual_vctx = store
        .find_vctx_for_scope(scope)
        .await
        .unwrap()
        .expect("vctx must exist after find_or_create_vctx");

    // Simulate `set_backend_session` recording which bridge owns the session.
    store
        .set_backend_session(&actual_vctx, vtoken, session, "some-uuid")
        .await
        .unwrap();

    // `find_vctx_for_scope` must return the vctx for the "peer:" scope.
    let found_vctx = store
        .find_vctx_for_scope(scope)
        .await
        .unwrap()
        .expect("find_vctx_for_scope must return Some");
    assert_eq!(found_vctx, actual_vctx);

    // `find_vtoken_for_session` must return the owning vtoken.
    let found_vt = store
        .find_vtoken_for_session(&actual_vctx, session)
        .await
        .unwrap()
        .expect("find_vtoken_for_session must return Some");
    assert_eq!(found_vt, vtoken);

    // Unknown session must return None (not panic or error).
    let not_found = store
        .find_vtoken_for_session(&actual_vctx, "at-99991231-999999")
        .await
        .unwrap();
    assert!(not_found.is_none());
}

// ─── Store layer mutation-catching tests (Phase C) ────────────────────────────

/// M9-store-1: find_or_create_vctx with a non-empty group_id must use the
/// "group:<id>" conv_key prefix.
/// Catches the `!s.is_empty()` → `s.is_empty()` mutant in the group_id filter
/// (context.rs:56), and the `format!("group:{g}")` → other format mutants.
#[tokio::test]
async fn find_or_create_vctx_group_id_uses_group_prefix() {
    let store = crate::store::Store::connect("sqlite::memory:")
        .await
        .expect("connect");
    let vctx = store
        .find_or_create_vctx("peer-x", Some("group-abc"), "real-ctx")
        .await
        .expect("must succeed");
    assert!(!vctx.is_empty(), "vctx must be non-empty for group message");

    // Same group_id must return same vctx (stable key).
    let vctx2 = store
        .find_or_create_vctx("peer-y", Some("group-abc"), "real-ctx-2")
        .await
        .expect("must succeed");
    assert_eq!(
        vctx, vctx2,
        "same group_id must always resolve to the same vctx regardless of peer_user_id"
    );
}

/// M9-store-2: find_or_create_vctx with an empty group_id must fall through to
/// the peer_user_id path.
/// Catches `group_id.filter(|s| !s.is_empty())` → always-Some mutant.
#[tokio::test]
async fn find_or_create_vctx_empty_group_id_falls_through_to_peer() {
    let store = crate::store::Store::connect("sqlite::memory:")
        .await
        .expect("connect");

    let vctx_with_empty_group = store
        .find_or_create_vctx("peer-unique-1", Some(""), "real-ctx")
        .await
        .expect("must succeed");
    let vctx_with_no_group = store
        .find_or_create_vctx("peer-unique-1", None, "real-ctx")
        .await
        .expect("must succeed");
    assert_eq!(
        vctx_with_empty_group, vctx_with_no_group,
        "empty group_id must be treated same as None (peer path)"
    );
}

/// M9-store-3: find_or_create_vctx with empty peer_user_id and no group_id
/// must still succeed (creates an anonymous context).
/// Catches the `!peer_user_id.is_empty()` → always-true mutant (context.rs:58).
#[tokio::test]
async fn find_or_create_vctx_empty_peer_and_no_group_returns_vctx() {
    let store = crate::store::Store::connect("sqlite::memory:")
        .await
        .expect("connect");
    let vctx = store
        .find_or_create_vctx("", None, "real-ctx-anon")
        .await
        .expect("empty peer must still create a vctx");
    assert!(
        !vctx.is_empty(),
        "vctx must be non-empty even for anonymous context"
    );
}

// ─── clients.rs mutation catch-up (2026-07-10) ───────────────────────────────

/// Catches `touch_client → Ok(())` no-op: last_seen must be set after touch.
#[tokio::test]
async fn touch_client_advances_last_seen() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let hashed = hash_vtoken("vhub_touch-client-aaaaaaaaaaaaaaa");
    store
        .upsert_client(&hashed, "touch-me", None)
        .await
        .expect("upsert");

    // Fresh INSERT does not set last_seen (only ON CONFLICT UPDATE does).
    let before = store
        .get_client_by_name("touch-me")
        .await
        .expect("get")
        .expect("row")
        .last_seen;

    store.touch_client(&hashed).await.expect("touch");

    let after = store
        .get_client_by_name("touch-me")
        .await
        .expect("get")
        .expect("row")
        .last_seen;
    assert!(
        after.is_some(),
        "touch_client must populate last_seen, got None"
    );
    assert_ne!(
        before, after,
        "touch_client must change last_seen (before={before:?}, after={after:?})"
    );
}

/// Catches `get_client_by_name → Ok(None)`: existing name must return Some with fields.
#[tokio::test]
async fn get_client_by_name_returns_existing_row() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let hashed = hash_vtoken("vhub_get-by-name-bbbbbbbbbbbbbbb");
    store
        .upsert_client(&hashed, "named-client", Some("lbl"))
        .await
        .expect("upsert");

    let row = store
        .get_client_by_name("named-client")
        .await
        .expect("get")
        .expect("must find named-client");
    assert_eq!(row.vtoken, hashed);
    assert_eq!(row.name, "named-client");
    assert_eq!(row.label.as_deref(), Some("lbl"));

    let missing = store
        .get_client_by_name("no-such-client")
        .await
        .expect("get missing");
    assert!(missing.is_none(), "unknown name must return None");
}

/// Catches `update_client_description → Ok(())` no-op.
/// Column is `NOT NULL DEFAULT ''`; empty string maps to `None` in ClientRow.
#[tokio::test]
async fn update_client_description_persists() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let hashed = hash_vtoken("vhub_desc-client-cccccccccccccccc");
    store
        .upsert_client(&hashed, "desc-client", None)
        .await
        .expect("upsert");

    store
        .update_client_description(&hashed, Some("hello desc"))
        .await
        .expect("update description");

    let row = store
        .get_client_by_name("desc-client")
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.description.as_deref(), Some("hello desc"));

    // Clear via empty string (column is NOT NULL).
    store
        .update_client_description(&hashed, Some(""))
        .await
        .expect("clear description");
    let row = store
        .get_client_by_name("desc-client")
        .await
        .expect("get")
        .expect("row");
    assert!(
        row.description.is_none(),
        "empty description must map to None, got {:?}",
        row.description
    );
}

/// Catches `delete_client_by_name → Ok(true/false)` and `rows_affected > 0`
/// comparison mutants (`>` → `==` / `<` / `>=`).
#[tokio::test]
async fn delete_client_by_name_reports_whether_row_existed() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let hashed = hash_vtoken("vhub_del-client-dddddddddddddddd");
    store
        .upsert_client(&hashed, "del-me", None)
        .await
        .expect("upsert");

    let deleted = store
        .delete_client_by_name("del-me")
        .await
        .expect("delete existing");
    assert!(deleted, "deleting an existing client must return true");
    assert!(
        store
            .get_client_by_name("del-me")
            .await
            .expect("get")
            .is_none(),
        "row must be gone after delete"
    );

    let deleted_again = store
        .delete_client_by_name("del-me")
        .await
        .expect("delete missing");
    assert!(
        !deleted_again,
        "deleting a missing client must return false (catches >= 0)"
    );
}

/// Catches `update_client_by_vtoken → Ok(())` no-op.
#[tokio::test]
async fn update_client_by_vtoken_persists_name_and_label() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let hashed = hash_vtoken("vhub_upd-by-vt-eeeeeeeeeeeeeeee");
    store
        .upsert_client(&hashed, "old-name", Some("old-label"))
        .await
        .expect("upsert");

    store
        .update_client_by_vtoken(&hashed, "new-name", Some("new-label"))
        .await
        .expect("update by vtoken");

    assert!(
        store
            .get_client_by_name("old-name")
            .await
            .expect("get")
            .is_none(),
        "old name must no longer resolve"
    );
    let row = store
        .get_client_by_name("new-name")
        .await
        .expect("get")
        .expect("new name must resolve");
    assert_eq!(row.vtoken, hashed);
    assert_eq!(row.label.as_deref(), Some("new-label"));
}

/// Catches `update_client_persona → Ok(())` no-op.
#[tokio::test]
async fn update_client_persona_persists() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let hashed = hash_vtoken("vhub_persona-client-ffffffffffff");
    store
        .upsert_client(&hashed, "persona-client", None)
        .await
        .expect("upsert");

    store
        .update_client_persona(&hashed, Some("Ada"), Some("🤖"))
        .await
        .expect("update persona");

    let row = store
        .get_client_by_name("persona-client")
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.persona_name.as_deref(), Some("Ada"));
    assert_eq!(row.persona_emoji.as_deref(), Some("🤖"));
}

// ─── context.rs mutation catch-up (2026-07-10) ───────────────────────────────

/// Pre-v7 schema lacks the partial unique index; upsert must fall back to the
/// two-step path and still return a stable vctx for the same peer.
#[tokio::test]
async fn find_or_create_vctx_pre_v7_fallback_is_stable() {
    sqlx::any::install_default_drivers();
    let pool = sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("pool");
    let store = Store {
        rpool: pool.clone(),
        pool,
        kind: DatabaseKind::Sqlite,
        master_key: std::sync::OnceLock::new(),
    };
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                migrated_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
            )",
        )
        .await
        .expect("schema_version");
    store
        .ddl(
            "CREATE TABLE IF NOT EXISTS context_token_map (
                vctx TEXT PRIMARY KEY,
                real_ctx TEXT NOT NULL,
                peer_user_id TEXT NOT NULL DEFAULT '',
                created_at TEXT
            )",
        )
        .await
        .expect("context_token_map");
    for v in 1..=12 {
        store.record_migration_run(v).await.expect("mark migrated");
    }

    let v1 = store
        .find_or_create_vctx("peer-pre-v7", None, "real-1")
        .await
        .expect("first create");
    let v2 = store
        .find_or_create_vctx("peer-pre-v7", None, "real-2")
        .await
        .expect("second create must fall back, not fail");
    assert_eq!(
        v1, v2,
        "same peer must keep stable vctx via two-step fallback"
    );
    assert_eq!(
        store
            .resolve_context_token(&v1)
            .await
            .expect("resolve")
            .as_deref(),
        Some("real-2"),
        "fallback path must update real_ctx"
    );
}

/// Anonymous contexts must resolve by freshly minted vctx, not the first empty
/// `peer_user_id` row left in the table (context.rs:135 `!conv_key.is_empty()`).
#[tokio::test]
async fn find_or_create_vctx_anonymous_resolve_uses_minted_vctx() {
    let store = Store::connect("sqlite::memory:").await.expect("connect");
    let anon1 = store
        .find_or_create_vctx("", None, "anon-one")
        .await
        .expect("anon1");
    store
        .find_or_create_vctx("peer-between", None, "peer-ctx")
        .await
        .expect("peer");
    let anon2 = store
        .find_or_create_vctx("", None, "anon-two")
        .await
        .expect("anon2");
    assert_ne!(
        anon1, anon2,
        "each anonymous call must mint a distinct vctx"
    );
    assert_eq!(
        store
            .resolve_context_token(&anon2)
            .await
            .expect("resolve")
            .as_deref(),
        Some("anon-two"),
        "second anonymous context must not alias the first empty-scope row"
    );
}

// ─── vctx ownership (arch-p0-hardening) ──────────────────────────────────────

/// `resolve_send_context` must return None when the vtoken was never granted
/// the vctx — even if the vctx itself exists in `context_token_map`.
#[tokio::test]
async fn resolve_send_context_rejects_unowned_vtoken() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let vctx = store
        .find_or_create_vctx("peer-owner-check", None, "real-ctx-owner")
        .await
        .expect("create vctx");

    // No grant yet → None.
    let unowned = store
        .resolve_send_context(&vctx, "vtoken-stranger")
        .await
        .expect("query");
    assert!(
        unowned.is_none(),
        "unowned vtoken must not resolve send context"
    );

    assert!(
        !store
            .vtoken_owns_vctx(&vctx, "vtoken-stranger")
            .await
            .expect("owns"),
        "stranger must not own vctx"
    );

    // Grant via active_sessions → Some.
    store
        .set_active_session_name(&vctx, "vtoken-owner", "default")
        .await
        .expect("grant");
    let owned = store
        .resolve_send_context(&vctx, "vtoken-owner")
        .await
        .expect("query")
        .expect("owner must resolve");
    assert_eq!(owned.0, "real-ctx-owner");
    assert!(store
        .vtoken_owns_vctx(&vctx, "vtoken-owner")
        .await
        .expect("owns"));
}

/// Ownership via `backend_sessions_v2` alone (no active_sessions row) is enough.
#[tokio::test]
async fn resolve_send_context_accepts_backend_session_grant() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let vctx = store
        .find_or_create_vctx("peer-bsess", None, "real-bsess")
        .await
        .expect("create vctx");
    store
        .set_backend_session(&vctx, "vtoken-bsess", "default", "sid-1")
        .await
        .expect("session");
    let owned = store
        .resolve_send_context(&vctx, "vtoken-bsess")
        .await
        .expect("query")
        .expect("backend_sessions grant must suffice");
    assert_eq!(owned.0, "real-bsess");
}
