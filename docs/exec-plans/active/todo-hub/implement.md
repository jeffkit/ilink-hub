# Implementation Log - Todo Hub

## Milestone 1: Fix SEC-002 (Pairing Code Replay Window & Exhaustion)
- **Status**: Completed
- **Changes**:
  - Updated `PairingSession::is_expired` in `src/hub/pairing.rs` to reset/check TTL as 60 seconds once the session is in the `Scanned` state.
  - Updated `PairingRegistry::mark_scanned` to set `created_at` to `Instant::now()` when transitioning to the `Scanned` state.
  - Modified `PairingRegistry::confirm` to immediately evict the confirmed session from `sessions` and store it in a temporary lookup (`confirmed_sessions`) with its own 60-second TTL to avoid memory exhaustion while allowing clients to retrieve the status.
  - Added concurrent session limit (maximum 10) in `PairingRegistry::create`.
  - Updated `create_pairing_qr` in `src/server/pairing.rs` to handle `PairingError::LimitExceeded` gracefully.
  - Added new unit tests in `src/hub/pairing.rs` to cover scanned TTL resets, session limits, and confirmed eviction/temporary lookup behavior.
- **Verification**:
  - Run `cargo fmt --check` -> Passed
  - Run `cargo clippy -- -D warnings` -> Passed
  - Run `cargo test` -> Passed
  - Run `cargo build` -> Passed
