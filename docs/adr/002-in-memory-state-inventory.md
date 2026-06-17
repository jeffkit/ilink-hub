# ADR-002: 内存状态清单与设计决策

> 状态：**已决策（记录现有设计）**  
> 日期：2026-06-17  
> 用途：明确哪些状态是纯内存、哪些有 DB 支撑、重启影响是什么

---

## 总览

iLink Hub 的运行状态分为三类：

| 类型 | 说明 | 重启影响 |
|------|------|---------|
| **纯内存** | 无持久化，进程退出即消失 | 丢失 |
| **内存 + DB 支撑** | DB 是权威数据源，内存是缓存/索引 | 可从 DB 恢复 |
| **纯 DB** | 所有读写直接操作 DB | 无影响 |

---

## 纯内存状态（重启即丢）

### 1. `InMemoryQueue` — 待处理消息队列

**位置**：`src/hub/queue.rs`  
**数据结构**：`DashMap<vtoken, Arc<PerClientSlot>>`，每个 slot 含 `Mutex<VecDeque<WeixinMessage>>`  
**容量**：每个 vtoken 最多 200 条（可通过 `ILINK_MAX_QUEUE_SIZE` 调整）  

**职责**：在 bridge `getupdates` 长轮询之间缓冲入站消息。

**重启影响**：
- 尚未被 bridge 轮询的消息**永久丢失**
- 丢失窗口通常 < 60 秒（bridge 重连速度）
- 在 bridge 长时间离线（如升级）时风险更高

**设计权衡**：
- 内存队列 latency 极低（无 DB 写入），适合互动型消息路由
- 持久化方案见 [ADR-001](./001-message-queue-persistence.md)

---

### 2. `QuoteRouteIndex` — 引用回复路由索引

**位置**：`src/hub/quote_route.rs`  
**数据结构**：
- `by_content: HashMap<u64, Vec<ContentEntry>>` — 出站消息内容哈希 → 后端路由
- 每个 `ContentEntry` 含 TTL（条目过期后通过 `spawn_quote_index_evictor` 每 5 分钟清理）

**职责**：用户引用回复某条 AI 消息时，将消息路由回发出该回复的后端（+ session）。

**三层 fallback（重启后依次降级）**：
```
1. QuoteRouteIndex（内存，最快）
   ↓ 未命中（重启后冷缓存）
2. messages 表 DB 查询（按 peer_user_id + content prefix）
   ↓ 未命中（消息表中无对应记录）
3. Footer 文本解析（最慢，仅适用于包含 "— backend · session" footer 的旧格式消息）
```

**重启影响**：
- 重启后 1-2 条引用回复会命中 DB fallback（稍慢）
- 若 messages 表缺失对应记录，引用路由失败（按普通消息处理，路由到默认后端）
- **不影响消息收发**，只影响「引用精准路由」功能

**设计权衡**：
- 不持久化的理由：QuoteRouteIndex 的条目本就有 TTL，重启后旧引用通常已过期
- messages 表作为 fallback 保证了新消息的引用路由在 DB 中有据可查

---

### 3. `PairingRegistry` — QR 配对会话

**位置**：`src/hub/pairing.rs`  
**数据结构**：`HashMap<code, PairingSession>`  
**TTL**：
- `Wait` 状态：600 秒
- `Scanned` 状态：60 秒（防止重放攻击 SEC-002）

**重启影响**：
- 正在进行中的 QR 配对**全部失效**，用户需重新发起配对
- 频率低（每次 bridge 首次注册时触发），影响可接受

**设计权衡**：配对码本质是短生命周期的一次性令牌，不值得持久化。

---

### 4. `PollTracker` — 并发轮询计数

**位置**：`src/hub/mod.rs`  
**数据结构**：`Mutex<HashMap<vtoken, usize>>`  

**重启影响**：重置为 0，允许 bridge 重新建立连接（**期望行为**）。

---

### 5. `ClientInfo.last_seen` — 客户端最后在线时间

**位置**：`src/hub/registry.rs`  
**类型**：`Option<Instant>`（进程本地时间，不可序列化）  

**重启影响**：
- 所有客户端 `online = false`，`last_seen = None`
- bridge 重连后（首次 `getupdates`）health checker 将其标记为 online
- 短暂离线（< 30 秒）不影响消息路由

---

## 内存 + DB 支撑（重启可恢复）

### 6. `ContextTokenMap` — 虚拟上下文令牌映射

**位置**：`src/hub/queue.rs`  
**数据结构**：三个 `LruCache<String, ...>`，容量各 50,000

```
v_to_record:  vctx → ContextRecord { real_ctx, peer_user_id, conv_key }
real_to_v:    real_ctx → vctx
conv_to_v:    conv_key → vctx        (每个微信用户/群的稳定 vctx)
```

**DB 支撑**：`context_token_map` 表（`vctx, real_ctx, peer_user_id`）

**重启后的冷启动行为**：
1. Hub 启动时预加载最近 500 条 `context_token_map` 记录到内存（`list_recent_context_tokens`）
2. 未在预加载范围内的用户首条消息触发 `find_vctx_for_peer` DB 查询，将结果 seed 进内存
3. 之后该用户的所有消息命中内存 LRU，无额外 DB 查询

**设计权衡**：
- LRU 而非 HashMap 的原因：微信账号可能服务数万用户，无界 HashMap 会造成 OOM
- 50,000 上限足以覆盖 95% 的活跃用户场景（每条记录约 200 字节，50K 条 ≈ 10MB）
- DB 作为 fallback 确保跨重启会话连续性

**已知风险（SEC-012）**：LRU 淘汰 + unique index 约束已修复，确保同一 real_ctx 不会映射到两个不同的 vctx。

---

### 7. `ClientRegistry` — 已注册客户端

**位置**：`src/hub/registry.rs`  
**数据结构**：`HashMap<vtoken, ClientInfo>` + `HashMap<name, vtoken>`  

**DB 支撑**：`clients` 表  

**重启恢复**：完整恢复（启动时调用 `load_clients_from_db`）。  
**内存独有**：`online` 状态（bridge 重连后自动恢复）、`last_seen Instant`。

---

### 8. `Router` — 每用户路由规则（`/use` 切换后端）

**位置**：`src/hub/router.rs`  
**数据结构**：`HashMap<from_user_id, vtoken>`

**DB 支撑**：`routing_state` 表  

**重启恢复**：完整恢复（启动时调用 `load_routing_state`）。

---

## 纯 DB 状态（重启无影响）

| 状态 | DB 表 | 说明 |
|------|-------|------|
| 客户端注册信息 | `clients` | vtoken、name、label |
| 用户路由规则 | `routing_state` | `/use` 后端切换记录 |
| 虚拟上下文映射 | `context_token_map` | vctx ↔ real_ctx ↔ peer_user_id |
| 后端 session | `backend_sessions_v2` | claude `--resume` session ID |
| 活跃 session | `active_sessions` | 每个 (vctx, vtoken) 的当前 session |
| 消息历史 | `messages` | user/assistant 消息，供引用路由 fallback |

---

## 总结：重启影响矩阵

| 功能 | 重启影响 | 恢复速度 |
|------|---------|---------|
| 消息路由（`/use` 规则） | ✅ 无影响 | 即时（启动时恢复） |
| 消息接收/转发 | ⚠️ 短暂中断 | bridge 重连后恢复（< 30s） |
| 未 poll 消息 | ❌ 丢失 | 不可恢复（见 ADR-001） |
| 引用回复路由 | ⚠️ 降级到 DB fallback | 1-2 条后恢复 |
| QR 配对 | ❌ 失效 | 用户需重新扫码 |
| Session 恢复（--resume） | ✅ 无影响 | DB 保存，bridge 重连后可用 |

---

## 相关设计决策

- [ADR-001](./001-message-queue-persistence.md) — 消息队列持久化方案选型
- [ADR-003](./003-sqlite-single-connection.md) — SQLite 单连接设计权衡
- [ADR-004](./004-fire-and-forget-persist.md) — fire-and-forget 持久化权衡
