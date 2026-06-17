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
