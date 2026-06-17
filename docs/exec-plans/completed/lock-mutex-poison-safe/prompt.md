# 目标

修复 ilink-hub 中热路径上的 std::sync::Mutex 误用和 .unwrap() 风险，防止 Tokio worker 被 panic：

## 问题

1. **src/hub/queue.rs**：`ContextTokenMap` 和 `PerClientSlot` 在 async dispatch/sendmessage 路径上使用 `std::sync::Mutex` 加锁并调用 `.unwrap()`。Mutex 中毒（mutex poisoning）会直接 panic 整个 Tokio worker。

2. **src/relay/ratelimit.rs:40**：`RateLimiter::allow` 使用 `lock().expect()`，中毒会 panic relay 路径。

## 修复方向

1. **queue.rs**：将热路径上的 `lock().unwrap()` 改为 poison-safe 写法：
   ```rust
   lock().unwrap_or_else(|e| e.into_inner())
   ```
   或考虑改用 `parking_lot::Mutex`（无 poison 语义，性能更好）。
   注意：如果引入 parking_lot，需在 Cargo.toml 加依赖。

2. **relay/ratelimit.rs**：同样改为 poison-safe 写法。

3. **src/ilink/upstream.rs:135-139**：`HeaderValue::from_str().unwrap()` 在每次请求时调用，改为 `map_err(|e| HubError::...)?` 或启动时预构建。

## 完成标准

- [ ] 热路径所有 `std::sync::Mutex::lock().unwrap()` 改为 poison-safe
- [ ] relay/ratelimit.rs 的 expect() 修复
- [ ] upstream.rs HeaderValue unwrap 修复
- [ ] `cargo clippy -- -D warnings` 零警告
- [ ] `cargo test` 全部通过
- [ ] `cargo build` 成功
- [ ] 补充相关单元测试（验证 mutex 中毒不会 panic）

## 非目标

- 不改变业务逻辑
- 不大规模重构 queue.rs 结构（只修目标问题）
- 暂不引入 tokio::sync::Mutex 替换（那是另一个更大的重构）
