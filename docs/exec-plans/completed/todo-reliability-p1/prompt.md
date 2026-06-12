修复 P1 可靠性修复：DB-01, E-03

## 待修复条目

  - [DB-01] SQLite AnyPool 多连接导致 SQLITE_BUSY
     文件：src/store/mod.rs:34-41
     问题：文件型 SQLite 使用 `AnyPool::connect(url)` 默认会建立多个连接。SQLite 文件锁不允许多个写事务并发，当 `persist_context_tokens_batch`（写事务）和 `get_active_session_name`（读）在不同连接上并发时，会触发 `SQLITE_BUSY`（错误码 5）。`:memory:` 已正确 `max_connectio
     修复方向：```rust   // 文件型 SQLite 同样 pin 到单连接，或设置 busy_timeout   let pool = if url.contains(":memory:") || url.starts_with("sqlite:") {       sqlx::pool::PoolOptions::<sqlx::Any>::new()           .max_connections(1)           .connect(url).await?   } else {       AnyPool::connect(url).await?   };   ```   或通过连

  - [E-03] relay 客户端无 shutdown 信号，进程关闭时强制 kill
     文件：src/relay/client.rs:18-29
     问题：`spawn_relay_client` 生成的 task 是无限重连 loop，没有 `watch::Receiver<bool>` shutdown 信号。Hub 关闭时该 task 被 tokio runtime 强制 drop，relay 服务端看到异常断开。对比 `spawn_health_checker`、`spawn_quote_index_evictor` 均正确使用了 `shut
     修复方向：在 `spawn_relay_client` 签名中加入 `shutdown_rx: watch::Receiver<bool>` 参数，loop 内的 sleep 和 run_session 均用 `tokio::select!` 包裹，命中 shutdown 时 `return`。

## 完成标准
- [ ] DB-01 修复已提交，相关测试通过
- [ ] E-03 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖