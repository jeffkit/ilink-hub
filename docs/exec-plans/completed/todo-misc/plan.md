# Execution Plan: Fix misc modules (S-03, D-01, D-02)

This plan outlines the milestones and validation steps for addressing the identified issues in the `misc` module.

## Milestones

### Milestone 1: Fix [S-03] Documentation Security Warnings
- **Tasks**:
  - Add security warning boxes next to `{{MESSAGE}}` config examples in [README.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/README.md) and [docs/bridge-config.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/bridge-config.md).
  - Explicitly explain that `{{MESSAGE}}` should not be used as part of a shell `-c` parameter due to shell injection risks, and recommend using `stdin: message` mode.
- **Validation Command**:
  Verify the warnings are present in both files:
  ```bash
  grep -E "stdin: message|MESSAGE" /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/README.md
  grep -E "stdin: message|MESSAGE" /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/bridge-config.md
  ```

### Milestone 2: Fix [D-01] sqlx Optional Driver Features
- **Tasks**:
  - Modify [Cargo.toml](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml) to introduce `[features]`.
  - Set `default = ["sqlite"]`.
  - Make `postgres` and `mysql` optional features for `sqlx`.
  - Update relevant documentation to reflect this breaking change.
- **Validation Command**:
  Verify cargo builds under different feature flags:
  ```bash
  cargo check --manifest-path /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml --no-default-features --features sqlite
  cargo check --manifest-path /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml --features postgres
  cargo check --manifest-path /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml --features mysql
  ```

### Milestone 3: Fix [D-02] Upgrade `rand` version to 0.9
- **Tasks**:
  - Update `rand` to version `"0.9"` in [Cargo.toml](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml).
  - Replace usage of `rand::thread_rng().gen::<u32>()` with `rand::random::<u32>()`.
  - Check compatibility with `ed25519-dalek` and its `rand_core` dependency.
- **Validation Command**:
  Check compilation of the project:
  ```bash
  cargo check --manifest-path /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml --all-targets
  ```

---

## E2E Checkpoints

### E2E-01: Full Tests & Clippy Validation
- **Goal**: Ensure that all changes compile cleanly, have zero new warnings, and pass all tests.
- **Validation Commands**:
  ```bash
  # Check for clippy warnings
  cargo clippy --manifest-path /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml --all-targets -- -D warnings
  
  # Run all test suites
  cargo test --manifest-path /Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml --all-targets
  ```
