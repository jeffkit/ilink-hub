# implement.md — mutation-test-coverage

## M1 — health.rs：spawn_health_checker 集成测试

### Decisions
- 使用 `#[tokio::test]` + `tokio::time::pause()` 控制虚拟时钟，避免真实等待 30 秒
- Store 初始化在 `pause()` 之前完成，规避 SQLite 连接池在暂停时钟下的 acquire 超时
- 通过两次 `tokio::time::advance` + `yield_now` 让后台任务执行：第一次推进 1s 让任务注册 sleep，第二次推进 CHECK_INTERVAL_SECS+1s 触发 sleep 到期

### Problems
- 无

### Outcome
- 测试 `health_checker_marks_stale_client_offline` 通过 ✅
- 捕获 `spawn_health_checker → ()` 变异体

---

## M2 — ratelimit.rs：eviction 阈值边界 + retain 谓词测试

### Decisions
- `eviction_threshold_is_strict_greater_than_10000`：插入恰好 10,000 个 key（不超过阈值），断言不触发驱逐
- `retain_keeps_fresh_buckets_and_evicts_stale_ones`：直接操作内部 map 写入 10,000 个 stale bucket，再调用 `allow` 触发驱逐，验证只有 fresh_key 保留

### Problems
- 无

### Outcome
- 两个测试均通过 ✅
- 捕获 `> 10_000` → `== / <` 以及 `< window` → `> / ==` 四个变异体

---

## M3 — queue.rs：push_shared 默认实现返回值传播

### Decisions
- 使用内部已有的 `TestQueue` 辅助结构（仅覆盖 `push()`，不覆盖 `push_shared`）
- 分别测试 `push` 返回 `Ok(false)` 和 `Ok(true)` 两种情况

### Problems
- 无

### Outcome
- `push_shared_default_propagates_false_from_push` 通过 ✅
- `push_shared_default_propagates_true_from_push` 通过 ✅
- 捕获 `push_shared → Ok(true)` 和 `push_shared → Ok(false)` 两个变异体
