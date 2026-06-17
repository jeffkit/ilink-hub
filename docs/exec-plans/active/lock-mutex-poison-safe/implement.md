# 实施记录：修复锁及 Unwrap 风险执行计划 (Poison-Safe Locks)

> 本文件按里程碑滚动记录。每个里程碑完成后追加一段：状态、关键改动、验证结果、commit 引用。

---

## M1: 修复 `src/hub/queue.rs` 锁安全 ─ done (2026-06-17)

### 状态
- **状态**：done
- **范围**：修改 `src/hub/queue.rs` 中的所有 `std::sync::Mutex` 使用，改为 poison-safe，并补充单元测试。
- **审查请求**：[reviews/m1/review-request.yaml](./reviews/m1/review-request.yaml)

### 关键改动

- 修改 `src/hub/queue.rs` 中的 `ContextTokenMap` 内部所有 `std::sync::Mutex::lock().unwrap()` 调用，替换为 `lock().unwrap_or_else(|e| e.into_inner())`。
- 修改 `src/hub/queue.rs` 中的 `PerClientSlot` 内部所有 `std::sync::Mutex::lock().unwrap()` 调用，替换为 `lock().unwrap_or_else(|e| e.into_inner())`。
- 在 `src/hub/queue.rs` 的 `queue_config_tests` 模块中新增 `test_mutex_poison_safe` 单元测试，通过显式在子线程获取锁后 panic 来毒化 `ContextTokenMap` 和 `PerClientSlot` 内部的 `Mutex`，然后验证主线程再次获取锁时能够安全返回数据而不发生 panic，确保了毒化安全性。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: fdc860c

## M2: 修复 `src/relay/ratelimit.rs` 锁安全 ─ done (2026-06-17)

### 状态
- **状态**：done
- **范围**：修改 `src/relay/ratelimit.rs` 中的 `RateLimiter::allow` 的锁，改为 poison-safe，并补充单元测试。
- **审查请求**：[reviews/m2/review-request.yaml](./reviews/m2/review-request.yaml)

### 关键改动

- 修改 `src/relay/ratelimit.rs` 中的 `RateLimiter::allow` 内部的 `self.inner.lock().expect(...)` 调用，替换为 `lock().unwrap_or_else(|e| e.into_inner())`。
- 在 `src/relay/ratelimit.rs` 的 `tests` 模块中新增 `test_ratelimit_poison_safe` 单元测试，通过显式在子线程获取锁后 panic 来毒化 `RateLimiter` 内部的 `Mutex`，然后验证主线程再次调用 `allow` 能够安全返回数据而不发生 panic，确保了毒化安全性。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: a5d2d59
