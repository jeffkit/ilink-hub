# ADR-003: SQLite 单连接池设计

> 状态：**已决策**  
> 日期：2026-06-17

---

## 背景

`Store::connect()` 对所有 SQLite URL（含文件型）使用 `max_connections(1)` 的连接池。

```rust
let pool = if url.starts_with("sqlite:") {
    sqlx::pool::PoolOptions::<sqlx::Any>::new()
        .max_connections(1)
        .connect(url)
        .await?
} else {
    AnyPool::connect(url).await?
};
```

---

## 问题

SQLite 是 **文件级写锁**：同一时刻只允许一个写事务，并发写请求需等待。

若允许多个连接：
- `persist_context_tokens_batch`（写事务，可能持锁数十毫秒）
- `get_active_session_name`（读）

在不同物理连接上并发时，读请求命中 `SQLITE_BUSY`（错误码 5），默认 busy timeout 触发失败，导致消息路由降级（无 session 信息）。

参见：[TODO.md DB-01（已修复）](../../docs/TODO.md)

---

## 决策

**将文件型 SQLite 也限制为 max_connections(1)**，强制所有 DB 操作串行。

代价：
- 所有 DB 查询串行执行，`dispatch_message` 热路径中的 DB 操作成为顺序瓶颈
- 高并发消息场景（>10 条/秒）下 DB 操作可能积压

收益：
- 彻底消除 `SQLITE_BUSY` 错误
- 无需手动设置 `busy_timeout`
- 简化并发控制——只需关注代码逻辑，无需担心锁竞争

---

## 已有缓解措施

- `build_hub_ext_for_vctx` 的两次 DB 查询加了 5 秒超时（`tokio::time::timeout`），超时时降级为 `"default"` session
- `persist_context_token` 使用信号量（`persist_sem`，容量 32）限制并发 fire-and-forget 任务数，防止池积压

---

## 适用范围与升级路径

| 部署规模 | 建议后端 | 说明 |
|---------|---------|------|
| 个人/小团队（<100 条/天） | SQLite（默认） | 单连接足够 |
| 中型（100-10,000 条/天） | SQLite + WAL mode 或 PostgreSQL | SQLite WAL 允许并发读+单写 |
| 大型（>10,000 条/天） | PostgreSQL | 完整并发支持，无单连接限制 |

启用 PostgreSQL 只需修改 `DATABASE_URL`：
```bash
DATABASE_URL=postgres://user:pass@localhost/ilink_hub ./ilink-hub serve
```

连接池自动扩展（不再受 `max_connections(1)` 限制）。

---

## 已知遗留问题

当前 `run_migrations` 仍使用内联 DDL 而非 `sqlx::migrate!`（H-1），在多环境部署时存在 schema 漂移风险。建议在 SQLite WAL 升级时同步迁移到 `sqlx::migrate!`。
