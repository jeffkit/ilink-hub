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

## M3: 修复 `src/ilink/upstream.rs` 中 `HeaderValue::unwrap()` ─ done (2026-06-17)

### 状态
- **状态**：done
- **范围**：修改 `src/ilink/upstream.rs` 中的 `HeaderValue::from_str(...).unwrap()`，改为 `?` 错误传播，并补充单元测试。
- **审查请求**：[reviews/m3/review-request.yaml](./reviews/m3/review-request.yaml)

### 关键改动

- 修改 `UpstreamClient::headers` 签名变更为 `fn headers(&self) -> Result<reqwest::header::HeaderMap>`。
- 将 `HeaderValue::from_str(...).unwrap()` 替换为 `HeaderValue::from_str(...)?`。
- 在 `notify_start`、`get_updates`、`send_message`、`send_typing`、`get_config`、`get_upload_url` 等异步方法中，使用 `?` 传播错误。
- 在 `src/ilink/upstream.rs` 的单元测试中新增 `headers_fail_with_invalid_token`，测试在非法 token 导致 `HeaderValue::from_str` 失败时能够正确返回错误而不 panic。

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

Commit: a0e2857

## M4: 静态代码检查与质量保障 ─ done (2026-06-17)

### 状态
- **状态**：done
- **范围**：运行 Clippy 静态代码检查，消除所有警告，并验证没有遗留的 `std::sync::Mutex` 误用。
- **审查请求**：[reviews/m4/review-request.yaml](./reviews/m4/review-request.yaml)

### 关键改动

- 运行 `cargo fmt --check`，代码格式符合规范。
- 运行 `cargo clippy -- -D warnings`，无任何静态检查警告或错误，验证未引入或残留锁/unwrap误用问题。
- 运行 `cargo test` 及相关构建、前端编译与 Tauri 校验命令，全部全绿通过。

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

Commit: 6a1c3db


