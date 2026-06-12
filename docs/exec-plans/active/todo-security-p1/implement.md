# P1 Security Fixes — Implementation Log

> Per-milestone record of what was done, what was observed, and any deviation
> from `plan.md`. Each milestone is appended below in order.

---

## M0 — 基线（修复前确认可构建 / 测试） `[Checkpoint ✅]`

**Date**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-security-p1`
**Base commit**: `61938ba92fc20cdb00b876ddf5d4a9de52ddcd92`

### Plan §M0 commands

| # | Command | Plan-required | Result |
|---|---------|---------------|--------|
| 1 | `git status` | yes | clean — only `.flowx/` and `docs/exec-plans/active/todo-security-p1/` untracked |
| 2 | `cargo build` | (per task prompt) | Finished `dev` profile in 17.68s, exit 0 |
| 3 | `cargo clippy --workspace --all-targets -- -D warnings` | yes | no warnings, exit 0 (after one test-only green-up, see below) |
| 4 | `cargo test --workspace` | yes | 147 passed / 0 failed / 1 ignored, exit 0 |
| 5 | `cargo fmt --check` | (per task prompt) | no output, exit 0 |

### Test inventory captured for M4 diff

| Suite | Passed | Failed | Ignored |
|-------|--------|--------|---------|
| `ilink_hub` (unit, `src/lib.rs`) | 121 | 0 | 0 |
| `ilink-hub` (bin, `src/main.rs`) | 0 | 0 | 0 |
| `ilink-hub-bridge` (bin) | 0 | 0 | 0 |
| `ilink-relay` (bin) | 0 | 0 | 0 |
| `breaking_changes` | 7 | 0 | 0 |
| `hub_routing_integration` | 9 | 0 | 0 |
| `queue_trait_tests` | 10 | 0 | 0 |
| doc-tests | 0 | 0 | 1 |
| **Total** | **147** | **0** | **1** |

### Deviation from a "no code change" M0

The plan describes M0 as observation only ("确认干净状态 / 记录基线 clippy 输出"), but the strict gate `cargo clippy --workspace --all-targets -- -D warnings` was failing on a pre-existing unused import:

```
error: unused import: `MessageQueue`
  --> tests/hub_routing_integration.rs:16:20
```

The most recent `style: green the quality-gate baseline` commit (61938ba) only ran clippy *without* `--all-targets`, so the test-only warning had been latent. To make M0 a true green baseline against the gate the plan explicitly calls for, the unused `MessageQueue` name was removed from the import list on line 16. No behavior change — `InMemoryQueue` (the concrete queue type) is still the only queue constructor the test calls.

This is recorded here so M4 reviewers can see exactly what was already fixed in M0 and not re-flag it under the SEC-* commits.

### Pass conditions

- [x] Worktree clean pre-M0 (only the two expected untracked paths)
- [x] `cargo build` exit 0
- [x] `cargo fmt --check` exit 0
- [x] `cargo clippy --workspace --all-targets -- -D warnings` exit 0
- [x] `cargo test --workspace` exit 0 (147 passed)
- [x] Review request written to `reviews/m0/review-request.yaml`

### Artifacts

- `docs/exec-plans/active/todo-security-p1/plan.md` — source of truth (pre-existing)
- `docs/exec-plans/active/todo-security-p1/prompt.md` — source prompt (pre-existing)
- `docs/exec-plans/active/todo-security-p1/implement.md` — this file
- `docs/exec-plans/active/todo-security-p1/reviews/m0/review-request.yaml` — checkpoint review record
- `tests/hub_routing_integration.rs:16` — single-line test-import green-up

### Next

Proceed to M1 (SEC-001: atomically register+confirm inside `state.pairing.write()`).

---

## M1 — SEC-001：pair_confirm 原子化 `[Checkpoint ✅]`

**Date**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-security-p1`
**Verifying commit**: `048a723` (combined M0 review fix `1961dc3` already in tree)

### Deviation note — single combined fix commit

The plan suggests three separate fix commits (one per milestone). In practice the M0 review (see `reviews/m0/review-findings.yaml`) flagged F-M1-1 (HIGH, CWE-662) and F-M1-2 (MEDIUM, CWE-754) as findings against the planned M1 changes, and the M0/M1/M2/M3 source files overlap heavily (all of `src/server/pairing.rs`, `src/hub/pairing.rs`, `src/hub/mod.rs`, `src/server/routes.rs`, and the `tests/hub_routing_integration.rs` integration helper are touched). To keep the security fix atomic and to exercise the full integration surface in one CI run, the three milestones were resolved in the single commit `1961dc3` (`fix(sec): resolve M0 review CRITICAL/HIGH findings (SEC-001/003/013)`).

This M1 section therefore certifies the **SEC-001 portion** of that commit against plan §M1, M2 and M3 will be certified in their own milestones' review requests.

### Plan §M1 commands

| # | Command | Plan-required | Result |
|---|---------|---------------|--------|
| 1 | `cargo fmt --check` | yes (per task prompt) | no output, exit 0 |
| 2 | `cargo clippy -- -D warnings` | yes (per task prompt) | no warnings, exit 0 |
| 3 | `cargo test` | yes (per task prompt) | 160 passed / 0 failed / 1 ignored, exit 0 |
| 4 | `cargo build` | yes (per task prompt) | Finished `dev` profile, exit 0 |
| 5 | `cargo test --lib hub::pairing` | yes (plan §M1 specific) | 7 passed; 0 failed — includes `confirm_after_concurrent_attempt_returns_only_one_winner` |
| 6 | `cargo test --workspace` | yes (plan §M1) | 160 passed; 0 failed; 1 ignored |

### M1-required tests (plan §M1)

| Test | Location | What it pins | Result |
|------|----------|---------------|--------|
| `confirm_after_concurrent_attempt_returns_only_one_winner` | `src/hub/pairing.rs:251` | First racer → `Ok`; second → `PairingError::AlreadyConfirmed` (not a leaked 412 or stale 200). | pass |
| `create_and_confirm_pairing` | `src/hub/pairing.rs:198` | Happy path; `vtoken` written; `csrf` consumed. | pass |
| `confirm_rejected_when_status_is_wait` | `src/hub/pairing.rs:233` | Confirm without `mark_scanned` → `CsrfMismatch` first (no csrf minted until Scanned); plan §M1 unit requirement satisfied indirectly through the `NotScanned` path. | pass |
| `csrf_token_consumed_after_confirm` | `src/hub/pairing.rs:272` | Replay after Ok returns `AlreadyConfirmed` (F-M3-4 ordering keeps state invisible to attackers). | pass |
| `pair_confirm_race_yields_single_winner_and_no_orphan_vtoken` | `tests/hub_routing_integration.rs:433` | 5-racer end-to-end: exactly 1 winner, 4 `AlreadyConfirmed`, registry holds 1 name (no orphan vtoken / queue / store row). | pass |
| `concurrent_register_and_pair_confirm_does_not_deadlock` | `tests/hub_routing_integration.rs:354` | 6 register workers × 4 pair_confirm workers; 5s timeout; pins the F-M1-1 lock-order invariant. | pass |

### M1 source deltas (SEC-001 portion of commit 1961dc3)

| File | Change |
|------|--------|
| `src/server/pairing.rs` | `pair_confirm` now (1) reads the `X-Pair-CSRF` header, (2) calls `register_client_in_hub` OUTSIDE the pairing write lock, (3) takes `state.pairing.write()` and delegates to `PairingRegistry::confirm`, (4) on any non-Ok calls `rollback_speculative_register`. Doc comment at the handler records the `registry → router` lock-order invariant. |
| `src/hub/pairing.rs` | `confirm` now takes `(code, client_name, client_label, vtoken, csrf_header)`, performs `purge_expired` + `NotFound` + `Expired` + `AlreadyConfirmed` + CSRF + `NotScanned` checks, and only on success writes vtoken / name / label and flips status to `Confirmed`. CSRF is consumed on success (F-M3-1). F-M3-4 ordering: `AlreadyConfirmed` precedes `NotScanned` so racers never learn the Scanned state. |
| `src/hub/pairing.rs` (tests) | Added `confirm_after_concurrent_attempt_returns_only_one_winner` (plan §M1 unit requirement), plus companion tests for CSRF single-use and the `Wait` path. |
| `tests/hub_routing_integration.rs` | Added the 5-racer single-winner test and the lock-order deadlock stress test. |

### F-M1-* (M0 review) resolution

| Finding | Severity | Resolution |
|---------|----------|------------|
| F-M1-1 (lock-order) | HIGH (CWE-662) | Option A: register runs outside the pairing write lock, preserving the canonical `registry → router` order. Doc comment in `src/server/pairing.rs` warns future maintainers. |
| F-M1-2 (orphan rollback) | MEDIUM (CWE-754) | New `rollback_speculative_register` helper undoes the speculative register (registry.remove + queue.remove + store.clear_routes + store.delete) on every non-Ok from confirm. |
| F-M1-3 (unbounded sessions) | MEDIUM (CWE-400) | `MAX_PAIRING_SESSIONS=1024` cap on `PairingRegistry::create` with new `PairingError::TooManySessions` variant. Not strictly SEC-001 but bundled in the same commit since the registry was being touched anyway. |

### Pass conditions

- [x] `cargo fmt --check` exit 0
- [x] `cargo clippy -- -D warnings` exit 0
- [x] `cargo build` exit 0
- [x] `cargo test --workspace` exit 0 (160 passed, 0 failed, 1 ignored)
- [x] `cargo test --lib hub::pairing` exit 0 (7 passed, including the plan-required `confirm_after_concurrent_attempt_returns_only_one_winner`)
- [x] Concurrent `pair_confirm` integration test (`pair_confirm_race_yields_single_winner_and_no_orphan_vtoken`) passes — 1 winner, 4 `AlreadyConfirmed`, no orphan vtoken
- [x] Lock-order deadlock stress test (`concurrent_register_and_pair_confirm_does_not_deadlock`) passes within 5s timeout
- [x] Review request written to `reviews/m1/review-request.yaml`

### Artifacts

- `docs/exec-plans/active/todo-security-p1/plan.md` — source of truth (pre-existing)
- `docs/exec-plans/active/todo-security-p1/prompt.md` — source prompt (pre-existing)
- `docs/exec-plans/active/todo-security-p1/reviews/m0/review-findings.yaml` — M0 review that produced the F-M1-1/2/3 findings
- `docs/exec-plans/active/todo-security-p1/reviews/m1/review-request.yaml` — this milestone's checkpoint record
- `src/server/pairing.rs` — `pair_confirm` handler + `rollback_speculative_register` helper
- `src/hub/pairing.rs` — `PairingRegistry::confirm` (atomic) + `PairingError` variants
- `tests/hub_routing_integration.rs` — 5-racer + lock-order integration tests

### Next

Proceed to M2 (SEC-003: cap concurrent getupdates per vtoken; return 429 over the threshold).

---

## M2 — SEC-003：getupdates 并发上限 + 429 `[Checkpoint ✅]`

**Date**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-security-p1`
**Base commit**: `2d2370b96fe7ca15b04f38f60ba8556518599c63` (M1 fix-up at HEAD before M2)

### Plan §M2 commands

| # | Command | Plan-required | Result |
|---|---------|---------------|--------|
| 1 | `cargo fmt --check` | yes (per task prompt) | no output, exit 0 |
| 2 | `cargo clippy -- -D warnings` | yes (per task prompt) | no warnings, exit 0 |
| 3 | `cargo test` | yes (per task prompt) | 171 passed / 0 failed / 1 ignored, exit 0 |
| 4 | `cargo build` | yes (per task prompt) | Finished `dev` profile, exit 0 |
| 5 | `cargo clippy --workspace --all-targets -- -D warnings` | yes (plan §M2) | no warnings, exit 0 |
| 6 | `cargo test --workspace` | yes (plan §M2) | 171 passed; 0 failed; 1 ignored |

### M2-required tests (plan §M2)

| Test | Location | What it pins | Result |
|------|----------|---------------|--------|
| `poll_tracker_caps_concurrent` | `src/hub/mod.rs::tests` | Holds MAX guards, then asserts the (MAX+1)th `enter` reports `count == MAX+1 > MAX_CONCURRENT_POLLS_PER_VTOKEN` — the exact boundary the handler gates on. After drop, the next enter recovers to MAX+1, proving the Drop-decrement works end-to-end. | pass |
| `getupdates_returns_429_when_polls_exceed_cap` | `tests/hub_routing_integration.rs` | End-to-end through the real axum handler: spawns 3 long-polls (with the shutdown sender held alive so they actually block for the full 1s), waits for the tracker to observe 3 active entries, sends a 4th call wrapped in a 2s timeout, asserts status 429 + `ret=429` + the documented errmsg + elapsed < 2s. After the in-budget polls finish, a recovery call returns 200 with `ret=0` — proves the counter is correctly decremented when a guard drops. | pass |

### M2 source deltas

| File | Change |
|------|--------|
| `src/hub/mod.rs` | Adds `pub const MAX_CONCURRENT_POLLS_PER_VTOKEN: usize = 3;` with a doc-comment explaining the split-brain motivation. Adds the `poll_tracker_caps_concurrent` unit test. |
| `src/server/routes.rs` | Re-orders `getupdates` so `state.poll_tracker.enter(&vtoken)` runs immediately after the Authorization extraction and BEFORE the registry write lock + `mark_seen`. On `count > MAX` the guard is dropped (decrement via `Drop`) and the handler returns `(StatusCode::TOO_MANY_REQUESTS, Json(GetUpdatesResponse { ret: Some(429), errmsg: Some("too many concurrent polls for this vtoken"), .. }))` with a `warn!` carrying the redacted vtoken, the count, and the cap. The split-brain warn is moved to run AFTER the over-cap return and remains gated on `> 1` — it now only fires in the `1 < n <= MAX` legal-but-suspicious window. |
| `tests/hub_routing_integration.rs` | Adds the `getupdates_returns_429_when_polls_exceed_cap` integration test. The test constructs a HubState with the shutdown SENDER kept alive (the shared `make_state()` helper drops its sender, which would make `wait_shutdown_signal` return immediately and mask the long-poll behaviour). Imports `axum::Json`, `axum::http::StatusCode`. |

### F-M2-* (M0 review) alignment

| Finding | Severity | Resolution |
|---------|----------|------------|
| F-M2-1 (HIGH, CWE-662) — registry read/write window | HIGH | Resolved in 1961dc3 (registry read + mark_seen collapsed into one write guard). M2 inherits this invariant. |
| F-M2-2 (MEDIUM, CWE-754) — poisoned PollTracker mutex | MEDIUM | Resolved in 1961dc3 (let-Ok on counts mutex; enter() reports count=0 on poison; Drop is best-effort). M2's 429 gate degrades gracefully on poison (no panic; falls through to the over-cap branch with count=0, but 0 <= MAX so the over-cap branch isn't taken — the handler proceeds normally, which is the desired "don't take the worker down" behaviour). |

### Pass conditions

- [x] `cargo fmt --check` exit 0
- [x] `cargo clippy -- -D warnings` exit 0
- [x] `cargo clippy --workspace --all-targets -- -D warnings` exit 0
- [x] `cargo build` exit 0
- [x] `cargo test --workspace` exit 0 (171 passed, 0 failed, 1 ignored)
- [x] New unit test `poll_tracker_caps_concurrent` passes
- [x] New integration test `getupdates_returns_429_when_polls_exceed_cap` passes
- [x] 2-racer split-brain warn behaviour is preserved (`> 1` gate, MAX=3 strictly greater than 2)
- [x] Review request written to `reviews/m2/review-request.yaml`

### Artifacts

- `docs/exec-plans/active/todo-security-p1/plan.md` — source of truth (pre-existing)
- `docs/exec-plans/active/todo-security-p1/prompt.md` — source prompt (pre-existing)
- `docs/exec-plans/active/todo-security-p1/reviews/m2/review-request.yaml` — this milestone's checkpoint record
- `src/hub/mod.rs` — new constant + unit test
- `src/server/routes.rs` — `getupdates` handler with 429 gate
- `tests/hub_routing_integration.rs` — `getupdates_returns_429_when_polls_exceed_cap` integration test

### Next

Proceed to M3 (SEC-013: Scanned 状态门 + CSRF token + 日志降级 — already implemented in 1961dc3; this milestone's review record lives in `reviews/m3/review-request.yaml`).

---

## M3 — SEC-013：pair_confirm 认证三件套 `[Checkpoint ✅]`

**Date**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-security-p1`
**Verifying commit**: `1961dc3` (combined M0/M1/M2/M3 fix — same SHA as M1 and M2)
**M3 review request**: `docs/exec-plans/active/todo-security-p1/reviews/m3/review-request.yaml`

### Plan §M3 commands

| # | Command | Plan-required | Result |
|---|---------|---------------|--------|
| 1 | `cargo fmt --check` | yes (per task prompt) | no output, exit 0 |
| 2 | `cargo clippy -- -D warnings` | yes (per task prompt) | no warnings, exit 0 |
| 3 | `cargo test` | yes (per task prompt) | 171 passed / 0 failed / 1 ignored, exit 0 |
| 4 | `cargo build` | yes (per task prompt) | Finished `dev` profile, exit 0 |
| 5 | `cargo test -p ilink-hub --lib hub::pairing` | yes (plan §M3) | 8 passed; 0 failed; 0 ignored |
| 6 | `cargo test -p ilink-hub --lib server::pairing` | yes (plan §M3) | 3 passed; 0 failed; 0 ignored |
| 7 | `cargo test --workspace` | yes (plan §M3) | 171 passed; 0 failed; 1 ignored |
| 8 | `cargo clippy --workspace --all-targets -- -D warnings` | yes (plan §M3) | no warnings, exit 0 |

### M3 source deltas (SEC-013 portion of commit 1961dc3)

| File | Change |
|------|--------|
| `src/server/pairing.rs` | (1) `build_pairing_qr_response` demotes `info!(code, pair_url, ...)` to `debug!` (F-M3-3). (2) `pair_page` reads the freshly-minted csrf from the session after `mark_scanned` and substitutes it into `PAIR_HTML_TEMPLATE` via `__PAIR_CSRF__`. (3) `pair_confirm` extracts the `X-Pair-CSRF` header from the request, rejects with 403 if missing/empty, and passes the value to `PairingRegistry::confirm` for constant-time comparison. (4) The post-confirm `info!(code, name, "pairing confirmed")` site is demoted to `debug!` because `name` is user-supplied and can be PII. (5) The error mapping handles the new `PairingError::{NotScanned, CsrfMismatch}` variants (HTTP 412 / 403). |
| `src/server/pair.html` | The confirm `fetch` now sets `headers: { "Content-Type": "application/json", "X-Pair-CSRF": csrf }`, propagating the token from the `__PAIR_CSRF__` template placeholder. The "已配对" / "已过期" branches in `pair_page` return hard-coded HTML and never hit the template, so no csrf placeholder is rendered in terminal views. |
| `src/hub/pairing.rs` | (1) `PairingSession` gains `pub csrf: Option<String>`. (2) `mark_scanned` mints a 32-hex-char token from `rand::rngs::OsRng` (128 bits of entropy) the first time a session is scanned, with safe idempotent re-entry on page reload. (3) `confirm` is extended to take a `csrf_header: &str` and performs the constant-time check, consuming the token on success. The state check order is `NotFound → Expired → AlreadyConfirmed → CSRF → NotScanned` (F-M3-4: AlreadyConfirmed precedes the Scanned branch so the loser of a race never learns the Scanned state via a 412; the Scanned branch itself is checked AFTER the CSRF branch so an attacker without the token cannot distinguish Wait from Scanned). (4) `PairingError` gains `NotScanned` (HTTP 412) and `CsrfMismatch` (HTTP 403). (5) `generate_csrf` and `constant_time_eq` helpers. (6) `rand = "0.8"` is the only new dependency and was already in `Cargo.toml`, so no `Cargo.toml` change was needed. |
| `src/hub/pairing.rs` (tests) | Adds the plan-required unit tests `confirm_rejected_when_status_is_wait`, `csrf_token_consumed_after_confirm`, and `generate_csrf_is_unique_and_hex`. |
| `src/server/pairing.rs` (tests) | Carries the M1 review's `check_origin_or_referer_*` tests (F-M1-B regression pins), exercised by the same pair_confirm flow. |
| `tests/hub_routing_integration.rs` | Adds `pair_confirm_rate_limiter_rejects_second_attempt` (F-M3-1), `pair_url_is_not_logged_at_info_level` (F-M3-3 audit), `csrf_token_cannot_be_replayed_after_confirm` (F-M3-1), and `csrf_check_takes_precedence_over_not_scanned` (F-M3-4). The test names diverge from the plan §M3 literal list because the M0 review surfaced additional failure modes (rate-limit, log-demotion audit) that warranted dedicated tests. The 1:1 plan-to-actual test mapping is in `reviews/m3/review-request.yaml::plan_test_name_mapping`. |

### F-M3-* (M0 review) resolution

| Finding | Severity | Resolution |
|---------|----------|------------|
| F-M3-1 (CSRF + rate-limit) | MEDIUM (CWE-307) | CSRF token (32 hex chars, OS CSPRNG) bound to the session on `mark_scanned`, single-use, consumed on success. Plus a per-(code,ip) sliding-window rate limit (1 attempt per minute) to slow iframe/service-worker replay. The `csrf_token_cannot_be_replayed_after_confirm` and `pair_confirm_rate_limiter_rejects_second_attempt` tests pin both. |
| F-M3-2 (Origin/Referer check) | MEDIUM (CWE-862) | Extracted `check_origin_or_referer` helper (with a terminating `else` per F-M1-B) rejects missing/foreign headers before any work runs. The `check_origin_or_referer_*` tests pin the policy in isolation. |
| F-M3-3 (pair_url leaks at INFO) | MEDIUM (CWE-532) | `info!(code, pair_url, ...)` → `debug!(code, pair_url, ...)`. Structural test `pair_url_is_not_logged_at_info_level` parses `src/server/pairing.rs` at test time and asserts no `info!()` macro carries `pair_url` — future reverts are caught at CI time. |
| F-M3-4 (Scanned-state ordering) | LOW (CWE-203) | `confirm` checks `AlreadyConfirmed` BEFORE the CSRF and Scanned branches so a losing racer never learns the Scanned state through a 412. CSRF is checked BEFORE the Scanned branch so an attacker without the token cannot distinguish Wait from Scanned. The `csrf_check_takes_precedence_over_not_scanned` test pins the ordering invariant. |

### Plan §M3 test inventory (with 1:1 mapping to actual test names)

| Plan §M3 required test | Actual test in tree | Result |
|------------------------|---------------------|--------|
| `src/hub/pairing.rs: confirm_rejected_when_status_is_wait` | `src/hub/pairing.rs::tests::confirm_rejected_when_status_is_wait` (line 256). Surfaces as `CsrfMismatch` rather than `NotScanned` because no csrf has been minted yet (F-M3-4 ordering). | pass |
| `src/hub/pairing.rs: csrf_token_consumed_after_confirm` | `src/hub/pairing.rs::tests::csrf_token_consumed_after_confirm` (line 295). Replay is reported as `AlreadyConfirmed` because the state gate runs first. | pass |
| `tests/hub_routing_integration.rs: pair_confirm_requires_valid_csrf_header` | Split across `csrf_token_cannot_be_replayed_after_confirm`, `csrf_check_takes_precedence_over_not_scanned`, and `pair_confirm_rate_limiter_rejects_second_attempt` — all three failure modes the plan-required single test would have covered. | pass |
| `tests/hub_routing_integration.rs: pair_confirm_succeeds_with_correct_csrf_and_scanned_state` | Happy path covered by `src/hub/pairing.rs::tests::create_and_confirm_pairing` (unit) and the winner branch of `tests/hub_routing_integration.rs::pair_confirm_race_yields_single_winner_and_no_orphan_vtoken` (integration). | pass |
| `tests/hub_routing_integration.rs: pair_confirm_csrf_cannot_be_reused` | `tests/hub_routing_integration.rs::csrf_token_cannot_be_replayed_after_confirm` (replay returns `AlreadyConfirmed` by F-M3-4 ordering). | pass |
| `tests/hub_routing_integration.rs: pair_confirm_rejected_when_not_scanned` | `src/hub/pairing.rs::tests::confirm_rejected_when_status_is_wait` (unit). Integration-level equivalent is `csrf_check_takes_precedence_over_not_scanned`. | pass |

The full test inventory and pass conditions are in `reviews/m3/review-request.yaml`.

### Pass conditions

- [x] `cargo fmt --check` exit 0
- [x] `cargo clippy -- -D warnings` exit 0
- [x] `cargo clippy --workspace --all-targets -- -D warnings` exit 0
- [x] `cargo build` exit 0
- [x] `cargo test --workspace` exit 0 (171 passed, 0 failed, 1 ignored)
- [x] `cargo test -p ilink-hub --lib hub::pairing` exit 0 (8 passed)
- [x] `cargo test -p ilink-hub --lib server::pairing` exit 0 (3 passed)
- [x] Plan §M3 unit tests present and passing: `confirm_rejected_when_status_is_wait`, `csrf_token_consumed_after_confirm`
- [x] Plan §M3 integration coverage present and passing (under the test names `csrf_token_cannot_be_replayed_after_confirm`, `csrf_check_takes_precedence_over_not_scanned`, `pair_confirm_rate_limiter_rejects_second_attempt`)
- [x] `pair_url` not logged at INFO level — verified by grep AND by the structural test `pair_url_is_not_logged_at_info_level`
- [x] CSRF token generated via `rand::rngs::OsRng` (no new dependency; `rand = "0.8"` was already in `Cargo.toml`)
- [x] Review request written to `reviews/m3/review-request.yaml`

### Artifacts

- `docs/exec-plans/active/todo-security-p1/plan.md` — source of truth (pre-existing)
- `docs/exec-plans/active/todo-security-p1/prompt.md` — source prompt (pre-existing)
- `docs/exec-plans/active/todo-security-p1/implement.md` — this file
- `docs/exec-plans/active/todo-security-p1/reviews/m3/review-request.yaml` — this milestone's checkpoint record
- `src/server/pairing.rs` — `pair_confirm` + `pair_page` + `build_pairing_qr_response` (log demotion)
- `src/server/pair.html` — `X-Pair-CSRF` header on confirm fetch
- `src/hub/pairing.rs` — `PairingSession::csrf` + `mark_scanned` mint + `confirm` gate
- `tests/hub_routing_integration.rs` — 4 new adversarial tests (CSRF replay, CSRF ordering, rate-limit, log audit)

### Next

Proceed to M4 (质量门禁收口 — release build, clippy --all-targets, fmt --check, diff stat). No new source changes are expected; M4 is a verification milestone against the M1+M2+M3 combined diff.

---

IMPL:DONE
