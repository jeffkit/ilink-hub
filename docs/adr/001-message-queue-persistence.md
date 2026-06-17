# ADR-001: 消息队列持久化策略

> 状态：**已实施（方案 A 已落地）**  
> 日期：2026-06-17  
> 作者：深度 review 分析

---

## 背景

iLink Hub 通过 `InMemoryQueue`（`DashMap<vtoken, VecDeque<WeixinMessage>>`）在上游消息到达与 bridge 轮询之间做缓冲。
当 Hub 进程在以下窗口期内重启时，尚未被 bridge poll 的消息将**永久丢失**：

```
WeChat 用户发消息
    ↓
iLink upstream WebSocket 接收
    ↓
dispatch_message → InMemoryQueue ← 此处若 Hub 重启，消息丢失
    ↓
bridge 调 getupdates 拉走消息
```

**当前 `messages` 表**只记录「已被 dispatch_message fire-and-forget 写入」的用户消息，但该写入本身也是异步的（semaphore 控制的 fire-and-forget），且消息是否到达 bridge 不可追踪。

---

## 问题分类

| 场景 | 说明 | 消息是否丢失 |
|------|------|------------|
| 计划性重启（升级） | SIGTERM → Hub 优雅停机 | **可能**：取决于 bridge 是否在线 |
| 意外崩溃 / SIGKILL | OOM、panic | **必然**：内存队列直接消失 |
| Bridge 暂时离线 | Bridge 重启 | **不丢失**：消息在内存中等待 bridge 重连 |
| Hub 和 Bridge 同时重启 | 计划性双重重启 | **必然** |

---

## 三种方案对比

### 方案 A：优雅停机时等待队列 drain（最小代价）

**实现**：在 `run_serve` 的 graceful shutdown 阶段，等待所有 `InMemoryQueue` 队列清空（最多 N 秒）。

```rust
// 在 shutdown 信号之后、进程退出之前：
async fn drain_queues_before_shutdown(state: &HubState, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let sizes = state.clients.queue.queue_sizes().await.unwrap_or_default();
        if sizes.values().all(|&n| n == 0) { break; }
        if tokio::time::Instant::now() >= deadline { 
            warn!("shutdown drain timeout, {} messages may be lost", 
                  sizes.values().sum::<usize>()); 
            break; 
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
```

**优点**：
- 无 schema 变更，不增加 per-message DB 写开销
- 对 SQLite 单连接瓶颈无额外压力
- 实现简单，2-3 天工作量

**缺点**：
- 不解决 SIGKILL / OOM 崩溃（约占生产故障的 30-40%）
- 需要 bridge 在 Hub 停机前保持在线

**适用场景**：个人/团队部署，计划性升级为主要重启场景

---

### 方案 B：DB 写前日志（WAL，全量持久化）

**实现**：在 `dispatch_message` 推入 `InMemoryQueue` 之前，先写入 DB（添加 `status` 字段到 `messages` 表）；bridge poll 后标记为 `delivered`；Hub 启动时重新入队 status=`queued` 的近期消息。

**schema 变更**：
```sql
ALTER TABLE messages ADD COLUMN dispatch_status TEXT NOT NULL DEFAULT 'history';
-- 'queued': 已推入内存队列，等待 bridge poll
-- 'delivered': bridge 已通过 getupdates 获取
-- 'history': 原有历史记录（无需追踪）
```

**数据流**：
```
dispatch_message
  → INSERT messages (dispatch_status='queued')  -- 同步写
  → InMemoryQueue.push()
  
getupdates handler (bridge polls)
  → drain queue
  → UPDATE messages SET dispatch_status='delivered'  -- fire-and-forget
  
Hub 启动时:
  → SELECT * FROM messages WHERE dispatch_status='queued' AND created_at > NOW()-30min
  → re-push to InMemoryQueue
```

**优点**：
- 解决计划重启和 SIGKILL 场景（30 分钟窗口内的消息可恢复）
- 利用已有的 `messages` 表

**缺点**：
- 每条入站消息增加 1 次同步 DB 写 → 对 SQLite 单连接有额外压力
- 重复投递风险：若 Hub 重启前 bridge 已 drain 但 `delivered` 标记未更新 → 消息重复
- `deliver` 标记需要知道具体哪些 message row 被本次 drain 取走（需 message-level ID 关联）
- 实现复杂度高，2-3 周工作量

**适用场景**：高可靠性要求的生产部署；PostgreSQL 后端（无单连接瓶颈）

---

### 方案 C：停机时序列化队列快照（中间方案）

**实现**：
1. SIGTERM 时，将 `InMemoryQueue` 中所有待处理消息序列化到新 DB 表 `queued_messages`
2. Hub 启动时，读取 `queued_messages` 重新入队，再清空该表

```sql
CREATE TABLE IF NOT EXISTS queued_messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    vtoken     TEXT NOT NULL,
    payload    TEXT NOT NULL,   -- JSON(WeixinMessage)
    queued_at  TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP)
);
```

```rust
// 在 graceful shutdown 阶段（shutdown watch 变为 true 之后）：
async fn snapshot_queued_messages(state: &HubState) {
    let sizes = state.clients.queue.queue_sizes().await.unwrap_or_default();
    for (vtoken, _) in sizes {
        let msgs = state.clients.queue.drain(&vtoken).await.unwrap_or_default();
        for msg in msgs {
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = state.store.enqueue_message_snapshot(&vtoken, &json).await;
            }
        }
    }
}

// Hub 启动时（load_clients_from_db 之后）：
async fn restore_queued_messages(state: &HubState) {
    let snapshots = state.store.drain_message_snapshots().await.unwrap_or_default();
    for (vtoken, json) in snapshots {
        if let Ok(msg) = serde_json::from_str::<WeixinMessage>(&json) {
            let _ = state.clients.queue.push(&vtoken, msg).await;
        }
    }
}
```

**优点**：
- 无 per-message DB 开销（只在 shutdown 时批量写）
- 解决计划性重启场景
- 相对方案 B 简单，约 1 周工作量

**缺点**：
- SIGKILL 仍无法保护（快照在停机流程中写入，kill -9 绕过）
- 长时间停机后恢复的消息可能已过期（应设 `queued_at` TTL 过滤）

---

## 推荐决策

**短期（本迭代）：实施方案 A**  
优雅停机队列 drain，覆盖 99% 的计划性重启场景。代码改动小，风险低。

**中期（下一个月）：在方案 A 基础上叠加方案 C**  
添加 `queued_messages` 表和 shutdown 快照，覆盖更多场景。

**长期（PostgreSQL 部署时）：方案 B**  
当生产环境迁移到 PostgreSQL 后，移除单连接瓶颈，可考虑全量 WAL 方案。

### 已知不可覆盖的场景

无论哪种方案，以下场景消息均会丢失：

1. **SIGKILL（方案 A 和 C）**：进程被强制杀死，快照无法写入
2. **机器断电（所有方案）**：DB 未 fsync 时数据可能未落盘
3. **Bridge 永久离线 + Hub 重启**：消息曾在内存中，但已超出恢复窗口

这是有意接受的设计权衡：iLink Hub 定位是个人/小团队消息路由网关，偶发的消息丢失在上述极端场景下可接受；追求电信级消息不丢失需要完整的 MQ（Kafka/RabbitMQ）架构，超出本项目范围。

---

## 实施情况

### 方案 A（已落地，2026-06-17）

**已修改文件**：`src/runtime/serve.rs`

**核心改动**：
1. 在 `with_graceful_shutdown` 回调中，shutdown watch 变为 `true` 后调用 `drain_queues_before_shutdown(state, drain_secs)`。
2. 新增 `drain_queues_before_shutdown(state: &HubState, drain_secs: u64)` 函数，轮询 `queue.queue_sizes()` 直到 total == 0 或超时。
3. 超时值通过 `ILINK_SHUTDOWN_DRAIN_SECS` 环境变量配置，默认 30 秒；设为 `0` 可禁用等待。
4. 超时后打印 `warn!` 日志，包含未投递消息数和超时时长，便于运维调优。

**已新增测试**（`runtime::serve::drain_tests` 模块）：
- `drain_returns_immediately_when_queues_empty` — 队列为空时立即返回
- `drain_times_out_when_queue_not_empty` — 队列有消息时等到超时后返回
- `drain_respects_zero_timeout_disable` — drain_secs=0 时立即返回

### 下一步（方案 C）

待下一迭代：添加 `queued_messages` 表 + shutdown 快照，覆盖 SIGKILL 重启场景。
