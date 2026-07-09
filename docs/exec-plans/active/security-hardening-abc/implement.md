## M1: CORS 接线 + vtoken 单次领取

### Decisions
- Chose `build_cors_layer()` in `build_router` over keeping `CorsLayer::permissive()` so `ILINK_CORS_ORIGINS` whitelist actually applies on the production bot API path.
- Chose `PairingRegistry::claim_confirmed_vtoken` (take-once, keep confirmed stub) over removing the confirmed session, so status/baseurl remain available on subsequent polls while `bot_token` is cleared after first claim.
- Wired `qrcode_status_json` to a write lock + claim (not read+clone) so long-poll and concurrent status readers cannot re-issue the bearer token for the CONFIRMED_TTL window.

### Problems & Solutions
- Problem: existing `cors_tests` only exercised a toy router with `build_cors_layer` in isolation, so a hard-coded permissive layer in `build_router` would still pass → Solution: added `build_router` + `HubState` integration tests hitting `/ilink/bot/get_bot_qrcode` with whitelist + evil Origin.
- Problem: `assert_eq!`/`assert_ne!` on `Option<&[u8]>` vs `Option<&[u8; N]>` failed to compile → Solution: compare via `.as_slice()`.

### Outcome
- Verification passed: `cargo fmt --all`, `cargo clippy -- -D warnings`, `cargo test --test cors_tests`, `cargo test pairing`, full `cargo test -- --test-threads=1` (pending/recorded in review-request), `cargo build`
- Commit: b859975
