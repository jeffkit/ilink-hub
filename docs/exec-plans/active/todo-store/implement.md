# Todo Store Implement Log

## M1 - Store Fixes (SYNC-02, DB-03, DB-02)

### Decisions

- **SYNC-02**: In `upsert_client`, query the old vtoken of the client first, then perform the client upsert. If the vtoken has indeed changed, perform `UPDATE routing_state SET active_vtoken = $new WHERE active_vtoken = $old` in the same transaction to prevent stale vtokens from causing message routing failures.
- **DB-03**: In `get_hub_ext_batch`, replace the row value `IN` clause syntax `WHERE (vctx, vtoken) IN (...)` and `WHERE (vctx, vtoken, session_name) IN (...)` with an equivalent chained `OR` clause matching pattern. This guarantees maximum compatibility across SQLite, PostgreSQL, and older MySQL versions (e.g. MySQL 5.7).
- **DB-02**: In `persist_context_tokens_batch`, slice the entries into batches of 50. Each chunk is committed in its own transaction, avoiding prolonged database write locks that block other concurrent writes during broadcast loops.

### Problems

- None. All changes were straightforward and compiled cleanly on the first try. Unit tests were successfully added to cover all three fixes.

### Outcome

- Implemented transaction-based routing state updates on client vtoken renewal.
- Rewrote batch database session lookup query syntax to be MySQL 5.7 compatible.
- Chunked bulk context token persistence transactions into max size 50.
- Added comprehensive unit tests:
  - `test_sync_02_upsert_client_updates_routing_state`
  - `test_db_03_get_hub_ext_batch_query`
  - `test_db_02_persist_context_tokens_batch_large`
- Formatted, checked, linted, tested, and verified build is fully green.
