# Database Migration Version Tracking Implement Log

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
