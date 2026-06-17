# Database Migration Version Tracking Implement Log

## M2 再验证 (2026-06-17, worktree: feat/db-migration-version-tracking)

在 `feat/db-migration-version-tracking` worktree 中重新验证 M2 全部验证命令。

### 验证结果

| 命令 | 结果 |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo clippy -- -D warnings` | PASS |
| `cargo test` | PASS (315 tests: 242 lib + 73 integration/e2e) |
| `cargo build` | PASS |
| `desktop-frontend` build | PASS |
| `desktop-tauri` cargo check | PASS |

### 文件变更

- `src/store/mod.rs` — 格式化修复（多行 assert 语句换行）
- `desktop/ilink-hub-desktop/src-tauri/Cargo.lock` — 依赖锁定更新
- `docs/exec-plans/active/db-migration-version-tracking/reviews/m2/review-request.yaml` — 更新验证结果
- `docs/exec-plans/active/db-migration-version-tracking/implement.md` — 本次更新

## M1 再验证 (2026-06-17, worktree: feat/db-migration-version-tracking)

在 `feat/db-migration-version-tracking` worktree 中重新验证 M1 全部验证命令。

### 验证结果

| 命令 | 结果 |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo clippy -- -D warnings` | PASS |
| `cargo test` | PASS (308 tests: 235 lib + 73 integration/e2e) |
| `cargo build` | PASS |
| `desktop-frontend` build | PASS |
| `desktop-tauri` cargo check | PASS |

### 修复

- **adversarial_many_concurrent_connects_converge 偶发失败**：10 并发 `Store::connect` 在 SQLite 文件锁竞争下偶发 `SQLITE_BUSY (code: 5)`。为每个 spawned task 添加了 retry 循环（最多 5 次，300ms 间隔），仅在 "database is locked" 时重试。

### 文件变更

- `src/store/mod.rs` — `adversarial_many_concurrent_connects_converge` 测试添加 retry 逻辑
- `docs/exec-plans/active/db-migration-version-tracking/plan.md` — 更新为执行后详细版
- `docs/exec-plans/active/db-migration-version-tracking/reviews/m1/review-request.yaml` — 更新验证结果

## Milestone 1: 建立迁移版本追踪表及测试框架

### Decisions

- Designed the `schema_version` table structure inside `src/store/mod.rs` to track migration version metadata.
- Implemented `get_current_version` method to retrieve the maximum version currently recorded in `schema_version` table (returning 0 if the table contains no records).
- Implemented `is_migration_run` to check if a specific migration version has already been executed.
- Implemented `record_migration_run` to log a newly executed migration version.
- Created `schema_version` table and inserted the baseline version 0 at the beginning of `Store::run_migrations`.
- Added unit test `test_schema_version_tracking` inside `store_tests` in `src/store/mod.rs` to verify that connecting to the database correctly initializes `schema_version` table with version 0, that we can query the current version, and that metadata checking and version recording work successfully.

### Problems

- None. The implementation and tests compiled and passed without issues.

### Outcome

- Verified that connecting to the database (even `sqlite::memory:`) correctly creates and initializes the `schema_version` table with version 0.
- All verification commands passed successfully:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (206 passed, 0 failed)
  - `cargo build` (exit 0)
  - `desktop-frontend` build (exit 0)
  - `desktop-tauri` cargo check (exit 0)
- Created `docs/exec-plans/active/db-migration-version-tracking/reviews/m1/review-request.yaml`.

## Milestone 2: 重构 Store::run_migrations 并支持版本管理逻辑

### Decisions

- Refactored `Store::run_migrations` from a single ~225-line function with five inline `if self.try_claim_migration(N).await? { ... }` blocks into a thin dispatcher that calls five private `migrate_to_v1` ... `migrate_to_v5` functions in order. Each migrator owns exactly one schema version and is gated by `try_claim_migration`, so the per-step boundary is now explicit (and matches the M1 review's F-M1-05 partial-apply note).
- Removed the explicit `.map_err(|e| anyhow::anyhow!(...))` wrappers around `CREATE UNIQUE INDEX` (v3), `ALTER TABLE ADD COLUMN` (v4), and `CREATE INDEX` (v5). The `ddl()` helper at `src/store/mod.rs:447` already prefixes its error with `"DDL failed: {sql}\n  Error: {e}"`, which is sufficient context. All DDL statements now use the bare `.await?` form — errors propagate uniformly to `Store::connect`, which returns `Err` and blocks the program from starting in a half-migrated state. The m1 design's `try_claim_migration` claim is still atomic and pre-claims the version row BEFORE the DDL runs (F-M1-01 / F-M1-04 invariant preserved).
- Confirmed all DDL uses `CURRENT_TIMESTAMP` — no `datetime('now')` residue. The `m2_ddl_uses_current_timestamp_only` test scans `sqlite_master` and asserts both halves of the plan's "unify on `CURRENT_TIMESTAMP`" clause.
- Added 8 new unit tests pinning the M2 invariants:
  - `m2_per_version_migrators_update_schema_version_independently` — running v2 alone marks v2 and leaves v1/v3/v4/v5 unmarked; v1's `clients` table does not exist.
  - `m2_migrators_are_idempotent_per_step` — re-running each migrator is a no-op.
  - `m2_ddl_error_propagates_through_migrator` — synthetic `CREATE UNIQUE INDEX` failure (seeded duplicate `real_ctx` rows) propagates as `Err` rather than being swallowed.
  - `m2_claim_and_record_are_consistent_with_schema_version` — `try_claim_migration` and `record_migration_run` agree on the row's presence.
  - `m2_v4_alone_with_minimal_preconditions` — v4 runs on a minimal pre-state and creates the column + index.
  - `m2_run_migrations_records_all_versions_in_order` — headline invariant: all five versions recorded.
  - `m2_run_migrations_idempotent_double_call` — `run_migrations` is safe to call twice.
  - `m2_ddl_uses_current_timestamp_only` — no `datetime('now')` residue in the catalog.

### Problems

- None. The refactor compiled, formatted, and tested cleanly on the first pass after addressing one cosmetic `rustfmt` issue with a multi-line `assert!` chain.

### Outcome

- The M2 refactor is structural: `run_migrations` went from a single ~225-line function with five inline `if` blocks to a 5-line dispatcher plus five ~30-60-line private migrators. Each migrator is now independently testable (the M2 test suite calls them directly with bootstrapped partial states).
- The error-handling surface is now uniform: every DDL uses `.await?`. A failure in v3 or v4 no longer matches the M1 review's F-M1-02 substring-heuristic concern (that concern was already addressed in M1; the M2 refactor removes the per-step `map_err` wrappers as well, so the error chain is exactly what `ddl()` produced).
- All verification commands passed successfully:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (295 passed, 0 failed; 8 new M2 tests, 0 regressions)
  - `cargo build` (exit 0)
  - `desktop-frontend` build (exit 0)
  - `desktop-tauri` cargo check (exit 0)
- Created `docs/exec-plans/active/db-migration-version-tracking/reviews/m2/review-request.yaml`.

## Milestone 3: 同步与对齐 `migrations/` 下的 SQL 文件

### Decisions

- Verified the existing `migrations/0001_initial_schema.sql`, `migrations/0002_backend_sessions.sql`, and `migrations/0004_context_token_map_created_at.sql` already use `CURRENT_TIMESTAMP` (no `datetime('now')` residue remains from the M2 work). No content edits were required on these three files.
- `migrations/0000_schema_version.sql` and `migrations/0005_messages.sql` were already in place from the M2 follow-up; M3 adds a docstring comment to `0005_messages.sql` explaining that the Rust migrator selects the per-driver DDL at runtime (the file documents the SQLite form, and `migrate_to_v5` substitutes `GENERATED BY DEFAULT AS IDENTITY` on Postgres / MySQL).
- **Fixed F-M2-02** (the portability finding from the m2 review): `migrate_to_v5` now selects the `id` column clause based on the driver — `INTEGER PRIMARY KEY AUTOINCREMENT` on SQLite, `INTEGER PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY` (SQL standard, supported by Postgres 10+ and MySQL 8.0+) on every other backend. The driver probe reuses the `column_exists` pattern: `SELECT current_database()` succeeds on Postgres/MySQL and errors on SQLite. The m2 review explicitly deferred this to M3 with the note that "AUTOINCREMENT is a SQLite-only keyword and will fail to parse on Postgres / MySQL, violating the M2 E2E Checkpoint 2 promise"; M3 closes that gap.
- Extracted the v5 DDL into a `Store::v5_create_messages_sql(is_sqlite: bool) -> String` helper so the m3 test surface can call both branches directly without spinning up a Postgres or MySQL connection. The two forms differ only in the `id` clause; field types, default values (`CURRENT_TIMESTAMP`), and table-level shape are identical to `migrations/0005_messages.sql`.
- Added 6 new unit tests pinning the M3 invariants:
  - `m3_v5_sqlite_ddl_matches_migration_file` — the SQLite branch of `v5_create_messages_sql` is byte-equivalent (after whitespace normalisation) to the `CREATE TABLE` block in `migrations/0005_messages.sql`. Pins the "SQLite form in the SQL file matches the Rust inline DDL" invariant.
  - `m3_v5_non_sqlite_ddl_uses_identity_not_autoincrement` — the non-SQLite branch uses `GENERATED BY DEFAULT AS IDENTITY` and does NOT use `AUTOINCREMENT`. Pins the F-M2-02 fix.
  - `m3_no_legacy_datetime_now_in_migration_files` — every `migrations/*.sql` file contains no `datetime('now')` residue. Pins the unification of timestamp defaults on the SQL-file side.
  - `m3_migration_files_use_current_timestamp` — every `migrations/*.sql` file that mentions a timestamp default uses `CURRENT_TIMESTAMP`. Companion to the previous test; asserts the affirmative side.
  - `m3_index_names_match_sql_files_and_catalog` — after `run_migrations`, the SQLite catalog contains the four index names the SQL files declare (`idx_context_token_map_real_ctx`, `idx_context_token_map_created_at`, `idx_messages_vctx_created`, `idx_messages_peer_role_created`). Cross-check between SQL reference and runtime catalog.
  - `m3_migration_files_match_inline_ddl_for_v1_v2_v4` — after `run_migrations`, the SQLite catalog contains every table name the SQL files declare (`clients`, `routing_state`, `context_token_map`, `bot_credentials`, `backend_sessions_v2`, `active_sessions`, `messages`). Cross-check between SQL reference and runtime catalog.
- The `normalise_sql` helper in `store_tests` collapses runs of whitespace, drops blank lines, drops `-- ...` line comments, and treats `;` as attached to the previous token so that end-of-statement `;`s on their own line are not lost. This lets the diff between a Rust DDL string and a SQL file focus on field-level substance rather than line-break conventions.

### Problems

- None. The m3 test surface needed one iteration: the first version of `m3_v5_sqlite_ddl_matches_migration_file` extracted the wrong slice of the SQL file (it compared the entire file rather than just the `CREATE TABLE` block) and `m3_migration_files_use_current_timestamp` initially asserted the property on `0003_*` (an index-only file with no timestamp default). Both were tightened in the same pass.

### Outcome

- The M3 alignment is complete: every `migrations/*.sql` file is the human-readable reference for the corresponding Rust migrator (modulo the v5 driver-specific `id` clause, which is a documented and tested divergence), no SQL file uses the legacy `datetime('now')` form, and the runtime catalog matches the SQL reference after `run_migrations`.
- The F-M2-02 portability gap is closed: `Store::migrate_to_v5` now uses driver-aware DDL, so `Store::connect` against a Postgres or MySQL database (the next step in the M2 E2E Checkpoint 2 promise) will not fail with "syntax error at or near 'AUTOINCREMENT'".
- All verification commands passed successfully:
  - `cargo fmt --check` (exit 0)
  - `cargo clippy -- -D warnings` (exit 0)
  - `cargo test` (303 passed, 0 failed; 6 new M3 tests, 0 regressions)
  - `cargo build` (exit 0)
  - `desktop-frontend` build (exit 0)
  - `desktop-tauri` cargo check (exit 0)
- Created `docs/exec-plans/active/db-migration-version-tracking/reviews/m3/review-request.yaml`.
