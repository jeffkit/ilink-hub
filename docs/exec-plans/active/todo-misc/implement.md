# Implementation Progress - Fix misc modules

## Progress Summary

- **Milestone 1: Fix [S-03] Documentation Security Warnings**
  - [x] Add security warning boxes next to `{{MESSAGE}}` config examples in README.md, docs/bridge/README.md, and docs/bridge-config.md.
  - [x] Explain command injection risks and recommend `stdin: message` mode.
  - [x] Create `review-request.yaml` for milestone m1.
- **Milestone 2: Fix [D-01] sqlx Optional Driver Features**
  - [ ] Introduce features in Cargo.toml
  - [ ] Make postgres/mysql optional
- **Milestone 3: Fix [D-02] Upgrade `rand` version to 0.9**
  - [ ] Upgrade rand dependency
  - [ ] Replace thread_rng usage

## Details for Milestone 1

- Updated [README.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/README.md) to include a configuration example and warning block.
- Updated [docs/bridge/README.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/bridge/README.md) to add a danger block detailing shell injection risk.
- Created [docs/bridge-config.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/bridge-config.md) with details on the security warnings for the `{{MESSAGE}}` configuration.

## Validation Results

- `cargo fmt --check`: Passed
- `cargo clippy -- -D warnings`: Passed
- `cargo test`: Passed
- `cargo build`: Passed
