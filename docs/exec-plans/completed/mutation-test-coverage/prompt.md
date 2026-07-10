# prompt.md — mutation-test-coverage

## 目标

为变异测试（cargo-mutants）中 **漏网（missed）** 的 9 条变异补写精准单元测试，
使变异得分从当前 93.2% 提升，确保以下三个模块的核心逻辑被测试充分覆盖：

1. `src/hub/health.rs` — `spawn_health_checker`：验证健康检查任务**确实启动**并在
   时钟推进后将失活客户端标记为离线。
2. `src/relay/ratelimit.rs` — `RateLimiter::allow`：补充两个边界条件测试：
   - 驱逐阈值精确为 `> 10_000`（不是 `>=`、`==`、`<`）
   - retain 谓词正确区分新鲜 / 过期 bucket（不是 `>` / `==`）
3. `src/hub/queue.rs` — `MessageQueue::push_shared` 默认实现：验证返回值严格
   传播自 `push()` 的布尔结果（不是常量 `Ok(true)` / `Ok(false)`）。

## 完成标准

- `cargo test` 全部通过（含新增测试）
- `cargo-mutants` 对上述三处漏网变异中至少 6/9 条变为 "caught"
- `cargo clippy -- -D warnings` 零警告
- `cargo fmt --all -- --check` 通过

## 非目标

- 不改动任何生产代码（仅添加测试）
- 不补齐 unviable 变异（`InMemoryQueue::with_limit`、`get_or_create` 等不可运行的）
- 不修复其他模块的变异覆盖问题（仅限 health / ratelimit / queue 三个文件）
