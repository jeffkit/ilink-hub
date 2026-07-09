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
- Commit: (pending)
