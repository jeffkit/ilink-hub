# ADR-004: Fire-and-Forget 上下文令牌持久化

> 状态：**已决策（接受当前设计，监控指标）**  
> 日期：2026-06-17  
> 关联：TODO.md C-01（已完成 metrics 部分）

---

## 背景

`dispatch_message` 在分发消息到 `InMemoryQueue` 之前，需要将 `(vctx, real_ctx, peer_user_id)` 映射关系持久化到 DB。这一操作采用 fire-and-forget 模式：

```rust
let sem = state.persist_sem.clone();
tokio::spawn(async move {
    let Ok(_permit) = sem.try_acquire() else {
        warn!("persist semaphore full, dropping context_token persist (forward)");
        metrics.persist_fire_and_forget_failures_forward.fetch_add(1, ...);
        return;
    };
    if let Err(e) = store.persist_context_token(&vctx2, &real2, &peer2).await {
        warn!(error = %e, "failed to persist context_token mapping");
        metrics.persist_fire_and_forget_failures_forward.fetch_add(1, ...);
    }
});
```

信号量容量 `MAX_CONCURRENT_PERSIST_TASKS = 32`。

---

## 问题

若信号量满（DB 操作积压）或 DB 操作失败，持久化**静默跳过**：

- `vctx ↔ real_ctx` 映射未写入 DB
- Hub 重启后，该用户下一条消息无法从 DB 恢复已有 vctx
- 新生成的 vctx 导致 backend session 断裂（用户看到「新对话」而非原有上下文延续）

---

## 决策：接受当前设计，添加可观测性

**理由**：

1. **内存 LRU 是主路径**：`ContextTokenMap` 容量 50K，绝大多数活跃用户命中内存，DB 持久化只是备份层
2. **影响范围有限**：只有 Hub 重启后 + 该用户恰好超出 LRU 的情况下才会丢失映射
3. **同步持久化代价高**：对每条入站消息做同步 DB 写会增加 `dispatch_message` 延迟，在 SQLite 单连接下更明显
4. **Metrics 可观测**：`persist_fire_and_forget_failures_forward` 和 `persist_fire_and_forget_failures_broadcast` 指标已接入 Prometheus

---

## 监控建议

在 `/metrics` 中监控以下指标，非零值需告警：

```promql
# fire-and-forget 失败率
rate(ilink_hub_persist_ff_failures_forward_total[5m]) > 0
rate(ilink_hub_persist_ff_failures_broadcast_total[5m]) > 0

# 信号量满（DB 积压）的信号
# 若 ff_failures > 0 且 messages_dispatched 增长，说明 DB 写入跟不上
```

---

## 已知限制

| 场景 | 影响 | 可接受？ |
|------|------|---------|
| 单条消息持久化失败 | Hub 重启后该用户可能新建 vctx | ✅ 是（低频，用户无感知） |
| 信号量满（大量并发） | 批量丢失持久化 | ⚠️ 需告警，触发时检查 DB 健康 |
| DB 持久失败（磁盘满） | 持续丢失所有新映射 | ❌ 需立即响应 |

---

## 未来优化方向

若部署迁移到 PostgreSQL，可考虑将 fire-and-forget 改为 **批量异步写**：
- 维护一个内存缓冲队列（容量 1000）
- 独立 task 每 1 秒批量刷写一次
- 既避免 per-message 同步写开销，又比信号量式 spawn 更有秩序
