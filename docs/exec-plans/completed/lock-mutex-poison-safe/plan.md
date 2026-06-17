# 修复锁及 Unwrap 风险执行计划 (Poison-Safe Locks)

该计划旨在修复 `ilink-hub` 热路径上的 `std::sync::Mutex` 误用和 `HeaderValue` 转换的 `unwrap()` 风险，防止 Tokio worker 被 panic 导致服务不可用。

## 修复目标

1. **`src/hub/queue.rs`**：`ContextTokenMap` 和 `PerClientSlot` 的热路径上使用 `std::sync::Mutex` 加锁并调用 `.unwrap()`。改成 poison-safe 形式：`unwrap_or_else(|e| e.into_inner())`。
2. **`src/relay/ratelimit.rs`**：`RateLimiter::allow` 的锁改成 poison-safe 形式。
3. **`src/ilink/upstream.rs`**：`HeaderValue::from_str(...).unwrap()` 改为传播 Result，由调用链顶层处理。

---

## 里程碑与验证步骤

### 里程碑 1: 修复 `src/hub/queue.rs` 锁安全
- **任务**
  - 修改 `src/hub/queue.rs` 中的所有 `std::sync::Mutex::lock().unwrap()` 为 `lock().unwrap_or_else(|e| e.into_inner())`。
  - 在 `queue.rs` 内部补充单元测试 `test_mutex_poison_safe`：显式在子线程中锁住并 panic 以毒化 Mutex，并在主线程中验证再次加锁时不会 panic 且可正常运作。
- **验证命令**
  ```bash
  cargo test --lib hub::queue::queue_config_tests
  ```

### 里程碑 2: 修复 `src/relay/ratelimit.rs` 锁安全
- **任务**
  - 修改 `src/relay/ratelimit.rs` 中的 `self.inner.lock().expect(...)` 为 `self.inner.lock().unwrap_or_else(|e| e.into_inner())`。
  - 在 `ratelimit.rs` 内部补充单元测试 `test_ratelimit_poison_safe`：验证 Mutex 被毒化后，调用 `RateLimiter::allow` 仍能安全获取锁而不 panic。
- **验证命令**
  ```bash
  cargo test --lib relay::ratelimit::tests
  ```

### 里程碑 3: 修复 `src/ilink/upstream.rs` 中 `HeaderValue::unwrap()`
- **任务**
  - 将 `UpstreamClient::headers` 签名变更为 `fn headers(&self) -> Result<reqwest::header::HeaderMap>`。
  - 使用 `HeaderValue::from_str(...)?` 替换 `HeaderValue::from_str(...).unwrap()`。
  - 在所有调用 `self.headers()` 的异步方法中，使用 `?` 传播错误（例如 `.headers(self.headers()?)`）。
  - 在 `upstream.rs` 的单元测试中，增加对非法 Token/UIN 生成的错误处理边界测试。
- **验证命令**
  ```bash
  cargo test --lib ilink::upstream::tests
  ```

### 里程碑 4: 静态代码检查与质量保障
- **任务**
  - 运行 Clippy 静态代码检查，消除所有警告，并验证没有遗留 of `std::sync::Mutex` 误用。
- **验证命令**
  ```bash
  cargo clippy --all-targets -- -D warnings
  ```

---

## E2E Checkpoint

在所有单体模块修改完毕且单元测试通过后，触发端到端验证，确保整个系统的原有业务流程和异常流程行为不受影响。

- **验证命令**
  ```bash
  cargo test --test e2e_wechat_simulation
  ```
- **验证项**
  - 确认微信模拟器 E2E 测试全部正常通过。
  - 确认所有原有集成测试（`tests/hub_routing_integration.rs` 等）全部通过：
    ```bash
    cargo test
    ```
