## M1: CORS 接线 + vtoken claim-window

### Decisions
- Chose `build_cors_layer()` in `build_router` over keeping `CorsLayer::permissive()` so `ILINK_CORS_ORIGINS` whitelist actually applies on the production bot API path.
- **Claim-window (not single-take):** `PairingRegistry::claim_confirmed_vtoken` keeps returning the same `vtoken` while `confirmed_at.elapsed() < VTOKEN_CLAIM_WINDOW` (120s) without clearing, so a lost `get_qrcode_status` response can be retried. After the window, the next claim permanently clears the token (closes the 24h re-poll steal hole) while retaining the confirmed stub for status/baseurl.
- Wired `qrcode_status_json` to a write lock + claim (not read+clone). Concurrent claims within the window are serialized by the write lock and each observes the same token.
- **f2 accepted residual risk:** the status poller remains unauthenticated; pair code is the capability. A concurrent racer who knows the code can still read the token during the claim window. Client-binding secret (issued at `get_bot_qrcode`) is deferred — not in this fix round.
- **f3 out of scope:** CORS default remains permissive when `ILINK_CORS_ORIGINS` is unset (OpenClaw browsers need CORS). Do not flip default to deny without plan approval.
- **f4:** covered cheaply by sequential double-claim-within-window unit test (write-lock serialization documented); full multi-thread race test deferred.

### Problems & Solutions
- Problem: existing `cors_tests` only exercised a toy router with `build_cors_layer` in isolation, so a hard-coded permissive layer in `build_router` would still pass → Solution: added `build_router` + `HubState` integration tests hitting `/ilink/bot/get_bot_qrcode` with whitelist + evil Origin.
- Problem: `assert_eq!`/`assert_ne!` on `Option<&[u8]>` vs `Option<&[u8; N]>` failed to compile → Solution: compare via `.as_slice()`.
- Problem (M1 adversarial f1 HIGH): pure single-take cleared vtoken on first poll; lost response + already-registered client → orphan + NameCollision wedge → Solution: claim-window semantics above.

### Outcome
- Verification passed: `cargo fmt --all`, `cargo clippy -- -D warnings`, `cargo test hub::pairing` (19), `cargo test --test cors_tests` (13), full `cargo test -- --test-threads=1`, `cargo build`
- Re-review: `reviews/m1/review-request.yaml` (fix round)

### M1 fix round (post adversarial NEEDS_FIX)
- Replaced single-take with `VTOKEN_CLAIM_WINDOW = 120s`
- Tests: reclaimable within window; cleared after window (backdated `confirmed_at`); wait/scanned still return None token
- Findings: f1 `fixed`; f2 `accepted`; f4 `partial` (sequential double-claim)

## M2: shell 硬拒绝 + 日志脱敏 + 桌面 loopback

### Decisions
- Replaced `warn_shell_injection_risk` with `reject_shell_injection_risk` → `Result`: only the dangerous combo (shell interpreter + `-c` + `{{MESSAGE}}` in args) fails load via `bail!`. Shell + `-c` without MESSAGE, and `stdin: message` profiles, still load.
- Added `redact_database_url` next to `redact_token` in `src/lib.rs`; startup log and `ServeOptions` Debug use the redacted form (password → `***`).
- Desktop `resolve_initial_listen_addr` validates `ILINK_HUB_ADDR` via `ensure_loopback_listen_addr` — only `127.0.0.1` / `localhost` / `::1` / bare port; `0.0.0.0` and LAN IPs Err.
- Desktop `hub_update_client` updated to new `update_client_in_hub` signature: read existing persona from registry and pass through (None would clear DB persona).

### Problems & Solutions
- Problem: desktop crate failed to compile against hub's 6-arg `update_client_in_hub` → Solution: preserve persona by reading registry before update (not pass None).
- Problem: `url::Url::parse` fails on some SQLite DSNs → Solution: fallback string mask for `user:pass@` authority; `sqlite::memory:` returned unchanged.

### Outcome
- Verification passed: `cargo fmt --all`, `cargo clippy -- -D warnings`, new unit tests (shell reject / redact / loopback), full `cargo test -- --test-threads=1`, `cargo build`, desktop `cargo test --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` (loopback filters)
- Commit: `af15970`

### M2 fix round (post adversarial NEEDS_FIX)

#### Decisions
- **f1:** `setup()` fallback extracted to `safe_listen_addr_on_resolve_error` — on any resolve Err, always use hardcoded `127.0.0.1:8765`; never re-read `ILINK_HUB_ADDR` (that undid the loopback check).
- **f2:** login success `println!` now uses `redact_database_url`.
- **f3:** `-c` detection via `arg_enables_shell_command_string` (covers `-lc`/`-ic`/`-xc`); MESSAGE placeholder scanned in args **and** env values; shell list +ksh/mksh/ash/busybox.
- **f5 (cheap):** `redact_database_url` also masks query keys password/passwd/pwd/sslpassword/key/secret/token → `***`.
- **f4 partial:** shell list expanded; python/node wrappers deferred (out of must-fix).

#### Problems & Solutions
- Problem: setup `unwrap_or_else` re-read rejected env → Solution: hardcoded safe default helper + unit test proving env stays `0.0.0.0` while fallback is loopback.

#### Outcome
- Findings: f1/f2/f3/f5 `fixed`; f4 `partial` (shell list only); f6/f7 left open (LOW).
- Verification: `cargo fmt --all -- --check`, `cargo clippy -- -D warnings`, full `cargo test -- --test-threads=1`, `cargo build`, desktop loopback/fallback tests (5 ok).
- Commit: `d662bde`
- Re-review: `reviews/m2/review-request.yaml` (fix round).

## M3: God 模块拆分（dispatcher + desktop lib）

### Decisions
- Split `src/bridge/dispatcher.rs` into `dispatcher/{mod,backoff,send,session,handle,tests}.rs` with mechanical moves; `pub(super)` for cross-submodule items; public API unchanged (`run_bridge`, `run_bridge_with_shutdown`, `BridgeStop`).
- Moved colocated tests to `dispatcher/tests.rs` so production `mod.rs` stays small (~157 lines); largest production file is `send.rs` (~509).
- Split desktop `lib.rs` into `listen_addr.rs`, `hub_commands.rs`, `bridge_profiles.rs`; `lib.rs` keeps HubController/spawn/run + invoke wiring via `pub(crate) use`.
- Extracted `test_support.rs` (`ScopedHome`, `PORT_OVERRIDE_LOCK`) so listen_addr and lib tests share HOME/port-override isolation.

### Problems & Solutions
- Problem: nested `mod tests` lost private parent imports after file split → Solution: explicit `use super::{...}` of `pub(super)`-visible items in `tests.rs`.
- Problem: sibling modules cannot see parent glob re-exports → Solution: explicit `use crate::listen_addr::…` / `Manager` imports in each submodule.
- Problem: listen_addr tests needed `ScopedHome` defined in lib tests → Solution: shared `test_support` module.

### Outcome
- Line counts: dispatcher 2515 → dir (mod 157 / send 509 / handle 283 / session 265 / backoff 73 / tests 1289); desktop lib 3062 → 1131 (−1931) + listen_addr 470 + hub_commands 528 + bridge_profiles 946.
- Verification: `cargo fmt --all`, `cargo clippy -- -D warnings`, `cargo test -- --test-threads=1`, `cargo build`, desktop `cargo test` + clippy `-D warnings`.
- Commits: `d372eab` (dispatcher), `8cf7bb5` (desktop)

## M4: 文档债 + 队列易失说明 + 归档过期 plans

### Decisions
- `overview.md`：仓库模块改为 `server/` `store/` `hub/` `bridge/` `ilink/` `relay/` `runtime/` `mcp/`；锁叙事改为 `tokio::sync` / `std::sync` / `DashMap` / `arc_swap`。
- `configuration.md`：`WEIXIN_BASE_URL` 默认监听澄清为 `127.0.0.1:8765`（env 可为 URL）；`ILINK_QUEUE_BACKEND=memory` 强调重启丢 pending、redis 未实现。
- `deployment-hardening.md`：§2 默认 loopback；§6 增加内存队列非持久化 bullet。
- `bridges/overview.md`：模块结构仍准确，未改。
- 归档：`mutation-test-coverage`、`mutation-test-coverage-p2`、`desktop-bridge-profiles`、`arch-cleanup-p1`（后者 status 标 superseded/completed-by-main，不重做）。
- `sendtyping-error-fix`：M5 仍 TODO，留 active；`todo-*` / `db-migration-version-tracking` 不动。

### Problems & Solutions
- Problem: `arch-cleanup-p1` status 全 pending 但 main 已有 N-01..N-07 → Solution: 更新 status 为 superseded，`git mv` 到 completed，明确勿重实现。

### Outcome
- Docs-only；无 `.rs` 变更，未跑 cargo 全量。
- Review: `reviews/m4/review-request.yaml`
