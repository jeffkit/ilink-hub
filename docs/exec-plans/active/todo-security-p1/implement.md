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
