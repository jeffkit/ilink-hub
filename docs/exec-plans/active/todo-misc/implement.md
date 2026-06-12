# Implementation Progress - Fix misc modules

## Progress Summary

- **Milestone 1: Fix [S-03] Documentation Security Warnings**
  - [x] Add security warning boxes next to `{{MESSAGE}}` config examples in README.md, docs/bridge/README.md, and docs/bridge-config.md.
  - [x] Explain command injection risks and recommend `stdin: message` mode.
  - [x] Create `review-request.yaml` for milestone m1.
- **Milestone 2: Fix [D-01] sqlx Optional Driver Features**
  - [x] Introduce features in Cargo.toml
  - [x] Make postgres/mysql optional
  - [x] Create `review-request.yaml` for milestone m2
- **Milestone 3: Fix [D-02] Upgrade `rand` version to 0.9**
  - [ ] Upgrade rand dependency
  - [ ] Replace thread_rng usage

## Details for Milestone 1

- Updated [README.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/README.md) to include a configuration example and warning block.
- Updated [docs/bridge/README.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/bridge/README.md) to add a danger block detailing shell injection risk.
- Created [docs/bridge-config.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/bridge-config.md) with details on the security warnings for the `{{MESSAGE}}` configuration.

## Details for Milestone 2

- Modified [Cargo.toml](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Cargo.toml) to introduce `[features]`, set `default = ["sqlite"]`, and map `sqlite`, `postgres`, and `mysql` to conditional `sqlx` driver features.
- Updated build scripts: [Dockerfile](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/Dockerfile), [.github/workflows/ci.yml](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/.github/workflows/ci.yml), and [.github/workflows/release.yml](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/.github/workflows/release.yml) to use `--all-features` during compilation.
- Updated documentation: [README.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/README.md), [docs/reference/configuration.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/reference/configuration.md), and [docs/guide/installation.md](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/guide/installation.md) to explain the new Cargo features requirement.
- Created [review-request.yaml](file:///Users/kongjie/projects/ilink-hub/.worktrees/todo-misc/docs/exec-plans/active/todo-misc/reviews/m2/review-request.yaml) for Milestone 2.

## Validation Results

- `cargo fmt --check`: Passed
- `cargo clippy --all-targets --all-features -- -D warnings`: Passed
- `cargo test --all-features`: Passed
- `cargo build --all-features`: Passed
- `cargo check --no-default-features --features sqlite`: Passed
- `cargo check --features postgres`: Passed
- `cargo check --features mysql`: Passed
