# plan.md — mutation-test-coverage

## 架构设计

纯测试添加，不修改生产代码。在三个现有文件的 `#[cfg(test)]` 模块内追加测试。

```
src/hub/health.rs           ← 追加 spawn_health_checker 集成测试
src/relay/ratelimit.rs      ← 追加 eviction 阈值 + retain 谓词边界测试
src/hub/queue.rs            ← 追加 push_shared 默认实现传播测试
```

## 里程碑

### M1 — health.rs：spawn_health_checker 集成测试

**目标**：验证当时钟推进 CHECK_INTERVAL_SECS+1 秒后，`last_seen` 过期的客户端
被健康检查任务标记为 offline。

**实现思路**：
- `#[tokio::test(start_paused = true)]` 控制虚拟时钟
- 用 `Store::connect("sqlite::memory:")` + `UpstreamClient::new` 构造 HubState
- `register()` 返回 `(plaintext, hashed_vtoken, is_new)`，用 `hashed_vtoken` 写 `last_seen`
- 设置 `last_seen[hashed_vtoken] = 0`（Unix epoch，必然过期）
- `mark_online(hashed_vtoken)` 先标记在线
- 调用 `spawn_health_checker(Arc::clone(&state))`
- `tokio::time::advance(31s)` + `tokio::task::yield_now()` 两次使任务运行
- 断言 `online_clients()` 为空

**验证命令**：
```bash
cargo test health::tests::health_checker_marks_stale_client_offline -- --nocapture
```

**E2E checkpoint**：not-ready（纯库单元测试，无 HTTP/UI 入口）
**E2E 判定依据**：e2e-protocol Step B — "纯库/CLI = NO"
**Visual Review**：not-needed

---

### M2 — ratelimit.rs：eviction 阈值边界 + retain 谓词测试

**目标**：
1. `eviction_threshold_is_strict_greater`：`window=0` 下插入恰好 10,000 个 key，
   断言 `inner.buckets.len() == 10_000`（驱逐未触发）。捕获 `>` → `==` 和 `>` → `<`。
2. `retain_keeps_fresh_evicts_stale`：直接向内部 map 插入 10,000 个 stale bucket
   （`window_start = now - 120s`，窗口 60s），再 `allow("fresh_key")` 触发驱逐，
   断言 `len == 1` 且只有 fresh_key 保留。捕获 `<` → `>` 和 `<` → `==`。

**验证命令**：
```bash
cargo test ratelimit::tests::eviction_threshold_is_strict_greater -- --nocapture
cargo test ratelimit::tests::retain_keeps_fresh_evicts_stale -- --nocapture
```

**E2E checkpoint**：not-ready（纯单元测试）
**E2E 判定依据**：e2e-protocol Step B — "纯库/CLI = NO"
**Visual Review**：not-needed

---

### M3 — queue.rs：push_shared 默认实现返回值传播

**目标**：在 `queue.rs` 的测试模块新增 `struct TestQueue`（仅实现必要方法，
不覆盖 `push_shared`），分别断言：
- `push()` 返回 `Ok(false)` 时，`push_shared()` 也返回 `Ok(false)`（捕获 `Ok(true)` 变异）
- `push()` 返回 `Ok(true)` 时，`push_shared()` 也返回 `Ok(true)`（捕获 `Ok(false)` 变异）

**验证命令**：
```bash
cargo test queue_config_tests::push_shared_default_propagates_push_result -- --nocapture
```

**E2E checkpoint**：not-ready（纯单元测试）
**E2E 判定依据**：e2e-protocol Step B — "纯库/CLI = NO"
**Visual Review**：not-needed

---

## 全局验证

```bash
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test
```
