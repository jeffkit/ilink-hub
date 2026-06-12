# Execution Plan: Fix Hub Module Issues

This plan outlines the milestones, verification commands, and E2E checkpoints for resolving the hub module issues (MEM-01, TO-02, S-01, C-01, A-01).

---

## Milestones

### Milestone 1: Fix [MEM-01] Broadcast path deep clone of `item_list`
*   **Description**: Modify `WeixinMessage.item_list` to use `Option<Arc<Vec<MessageItem>>>` instead of `Option<Vec<MessageItem>>` to avoid deep cloning for every online client. Update the `sendmessage` handler to clone the Arc or perform copy-on-write if modifications are needed.
*   **Verification Command**:
    ```bash
    cargo test --lib hub::tests
    ```

### Milestone 2: Fix [TO-02] DB queries timeout in `build_hub_ext_for_vctx`
*   **Description**: Wrap DB queries (`get_active_session_name` and `get_backend_session`) inside `build_hub_ext_for_vctx` with `tokio::time::timeout(Duration::from_secs(5), ...)` and handle timeouts gracefully.
*   **Verification Command**:
    ```bash
    cargo test --lib hub::tests
    ```

### Milestone 3: Fix [S-01] vtoken exposure in debug logs
*   **Description**: Redact `vtoken` in `src/hub/router.rs:159` to print only the first 8 characters (e.g., `%&vtoken[..vtoken.len().min(8)]`).
*   **Verification Command**:
    ```bash
    cargo test --lib hub::router::tests
    ```

### Milestone 4: Fix [C-01] Broadcast persist fire-and-forget window
*   **Description**: Add a metrics counter to record fire-and-forget persistence failures and document this design trade-off in the README.
*   **Verification Command**:
    ```bash
    cargo test && grep -q "metrics" src/hub/mod.rs
    ```

### Milestone 5: Fix [A-01] Refactor `HubState` struct
*   **Description**: Split the monolithic `HubState` struct into cohesive sub-states (e.g., `IlinkConnState`, `RoutingState`) to restrict direct access to its fields.
*   **Verification Command**:
    ```bash
    cargo clippy --all-targets
    ```

---

## E2E Checkpoints

### [E2E-01] Code Quality and Compilation Check
*   **Description**: Verify that the changes do not introduce compilation errors or new clippy warnings.
*   **Verification Command**:
    ```bash
    cargo clippy --all-targets --all-features -- -D warnings
    ```

### [E2E-02] Full Unit & Integration Test Suite Verification
*   **Description**: Run all tests in the repository to guarantee no regressions are introduced in the hub or adjacent modules.
*   **Verification Command**:
    ```bash
    cargo test --all-targets
    ```
