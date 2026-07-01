# 提案：iLink Hub 多 Agent 协同

| 项 | 值 |
|---|---|
| 状态 | **Active — Phase 1/2 设计中** |
| 作者 | iLink Hub 团队 |
| 创建 | 2026-06-14 |
| 更新 | 2026-07-01 |
| 前置依赖 | 已实现的 `@<后端>` 快捷指令、引用回复路由、`(vctx, vtoken, session)` 会话隔离、origin footer |

---

## 1. 背景与现状

iLink Hub 当前已支持（Phase 0 ✅ 已验证）：

- 用 `@<名称>` 让指定后端处理一条消息
- 用引用回复精确续会话
- 用 `/use` 切换活跃后端

**已验证**：用户可以在同一个微信里手动协调多个 Agent（人当路由器）。现在进一步让 **Agent 之间自动协调**。

---

## 2. 核心设计

### 2.1 分层：通讯层 vs 展示层

| 层 | 机制 | 说明 |
|----|------|------|
| **通讯层** | MCP 工具调用 | Agent 通过 Hub 暴露的 MCP Server 发现和调用其他 Agent |
| **展示层** | 微信 `@agent` 消息 | Hub 把 Agent 间的调用/回复以 `@名称` 格式实时展示给用户 |

两者解耦：通讯走 MCP（可靠、结构化），展示走微信消息（透明、可读）。

### 2.2 Hub MCP Server

Hub 新增一个 MCP Server 端点，暴露以下工具：

#### 工具 1：`list_agents`

```json
{
  "name": "list_agents",
  "description": "获取当前 iLink Hub 上在线的 Agent 列表及其简介",
  "inputSchema": {
    "type": "object",
    "properties": {}
  }
}
```

返回：
```json
{
  "agents": [
    { "name": "code-reviewer", "description": "专注代码审查与安全分析" },
    { "name": "researcher", "description": "网络搜索与信息整合" }
  ]
}
```

#### 工具 2：`call_agent`

```json
{
  "name": "call_agent",
  "description": "向另一个 Agent 发送消息并等待回复（同步阻塞）",
  "inputSchema": {
    "type": "object",
    "properties": {
      "agent_name": { "type": "string", "description": "目标 Agent 的注册名称" },
      "message": { "type": "string", "description": "发送给目标 Agent 的消息内容" }
    },
    "required": ["agent_name", "message"]
  }
}
```

返回：
```json
{
  "response": "目标 Agent 的回复内容"
}
```

---

## 3. call_agent 内部流程

```
Agent A (Claude Code / Cursor) 调用 MCP tool: call_agent('code-reviewer', 'task')
         │
         ▼
Hub MCP Handler:
  1. 生成 correlation_id
  2. 向微信发送: @code-reviewer\ntask  (展示层，标注来自 A)
  3. 构造合成入站消息 → 推入 code-reviewer 的队列
  4. 在 a2a_pending 表中登记: correlation_id → oneshot::Sender
  5. 等待 (async, 最长 120s)
         │
         ▼
code-reviewer Bridge 收到合成消息 → 处理 → sendmessage
         │
         ▼
Hub sendmessage Handler:
  - 检测 ilink_hub_ext.a2a_correlation_id 是否有值
  - 若有: 向微信发送 code-reviewer 的回复，同时 resolve oneshot
  - 若无: 正常转发给 iLink upstream
         │
         ▼
MCP call_agent 返回 code-reviewer 的回复给 Agent A
         │
         ▼
Agent A 拿到结果，继续生成最终回复 → 正常走 sendmessage → 发到微信
```

### 关键点

- **Agent A 的子进程在整个 call_agent 期间保持运行**（HTTP 长连接阻塞等待），这和 Claude Code / Cursor Agent 调用其他 MCP 工具的方式完全一样。
- **不引入多 Turn**：从用户角度，A 发出一条消息，最终收到 A 的最终回复，中间的 A↔B 交互自动发生。
- **微信全程透明**：A 调用 B 时 Hub 立即发消息到微信（用户看到），B 回复时也立即发消息（用户看到）。

---

## 4. 微信展示效果

```
用户：帮我审查这段 Rust 代码

— claude：
(正在向 @code-reviewer 咨询...)
@code-reviewer：请检查以下代码的并发安全性

— code-reviewer：
分析完成，发现 2 个问题：
1. 第 38 行 Mutex 存在锁逆序风险
2. 第 52 行有 TOCTOU 问题

— claude：
综合 code-reviewer 的分析，修改建议如下：...
```

其中：
- A 的 `call_agent` 触发时，Hub 立即向微信发一条：`@code-reviewer\n[任务内容]`，带 claude 的 footer
- B 的回复通过 `sendmessage` 正常发到微信，带 code-reviewer 的 footer
- A 拿到 B 的结果后，继续生成最终回复，正常发到微信

---

## 5. 技术实现

### 5.1 Hub 侧新增内容

#### A. MCP Server 路由（`src/server/mod.rs` + 新模块）

Hub 的 Axum server 新增路由，支持 MCP Streamable HTTP Transport（2025-11-05 规范）：

```
POST /hub/mcp            ← MCP JSON-RPC endpoint（带 auth）
GET  /hub/agents/list    ← 简化 REST 版（给 SDK 用，可选）
```

认证：Bearer vtoken（与现有 `sendmessage` 等一致）。

#### B. A2A 状态（`src/hub/state.rs`）

在 `HubState` 中新增：

```rust
pub struct HubState {
    // ...existing fields...
    pub a2a: Arc<A2AState>,
}

pub struct A2AState {
    /// 等待中的 call_agent 请求: correlation_id → 回传通道
    pub pending: Mutex<HashMap<String, oneshot::Sender<String>>>,
}
```

#### C. HubExt 新增字段（`src/hub/dispatch.rs` 或相关类型）

```rust
pub struct HubExt {
    // ...existing fields...
    /// 若本条消息是 A2A 合成消息，此字段存回传 correlation_id
    pub a2a_correlation_id: Option<String>,
    /// 若本条消息是 A2A 合成消息，此字段存发起方名称（用于展示 "@来自 X"）
    pub a2a_caller: Option<String>,
}
```

#### D. sendmessage 修改（`src/server/routes.rs`）

在发送 upstream 之前，检测 HubExt 中的 `a2a_correlation_id`：

```rust
if let Some(correlation_id) = hub_ext.a2a_correlation_id {
    // 从 pending 表中取出回传通道
    if let Some(tx) = state.a2a.pending.lock().await.remove(&correlation_id) {
        let _ = tx.send(response_text.clone());
        // 不 return，继续正常 upstream 发送（让用户也看到 B 的回复）
    }
}
```

#### E. MCP call_agent Handler

```rust
async fn handle_mcp_call_agent(
    state: Arc<HubState>,
    caller_vtoken: String,
    agent_name: String,
    message: String,
    caller_vctx: String,
) -> Result<String, A2AError> {
    // 1. 查找目标 vtoken
    let target_vtoken = state.clients.registry.lock().await
        .find_by_name(&agent_name)
        .ok_or(A2AError::AgentNotFound(agent_name.clone()))?;

    // 2. 向微信推送 A→B 的展示消息（带 caller 的 footer）
    post_delegation_to_wechat(&state, &caller_vctx, &caller_vtoken, &agent_name, &message).await?;

    // 3. 生成 correlation_id，注册到 pending 表
    let correlation_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<String>();
    state.a2a.pending.lock().await.insert(correlation_id.clone(), tx);

    // 4. 构造合成入站消息推给 B
    let synthetic = build_a2a_synthetic_message(
        &caller_vctx, &agent_name, &message, &correlation_id
    );
    state.clients.queue.push(&target_vtoken, synthetic).await?;

    // 5. 等待 B 的回复（超时 120s）
    tokio::time::timeout(Duration::from_secs(120), rx)
        .await
        .map_err(|_| A2AError::Timeout)?
        .map_err(|_| A2AError::ChannelClosed)
}
```

#### F. A2A 会话隔离

合成消息的 session_name 使用独立命名空间：

```
a2a-{caller_name}-{timestamp}    示例: a2a-claude-20260701-143022
```

使用现有 `backend_sessions_v2` 表，无需新表。

### 5.2 Node SDK 新增（`sdk/node/src/`）

为使用 `createProfile` 的 Node.js 场景提供便利方法：

```typescript
// ctx 新增 agents 命名空间
interface ProfileContext {
  // ...existing fields...
  agents: {
    list(): Promise<AgentInfo[]>;
    call(name: string, message: string): Promise<string>;
  };
}
```

内部实现：直接 HTTP POST 到 Hub 的 REST API（`/hub/agents/list`、`/hub/mcp`），使用 Bridge 已知的 Hub URL + vtoken 鉴权。

**新增 env var**（Bridge 向子进程传递）：
- `ILINK_HUB_BASE_URL`：Hub 的 HTTP 地址（如 `http://localhost:8765`）
- `ILINK_VTOKEN`：本 backend 的 vtoken（用于 MCP 调用鉴权）

### 5.3 Claude Code / Cursor Agent 配置

对于使用 Claude Code 或 Cursor 的 profile，在其 MCP 配置中添加 Hub MCP Server：

```json
{
  "mcpServers": {
    "ilink-hub": {
      "url": "http://localhost:8765/hub/mcp",
      "headers": {
        "Authorization": "Bearer {ILINK_VTOKEN}"
      }
    }
  }
}
```

profile 脚本可在启动 Claude Code 前动态写入此配置（读取 `ILINK_HUB_BASE_URL` + `ILINK_VTOKEN` env var）。

---

## 6. 分阶段落地

### Phase 0 ✅（已完成）
人当路由器，手动转发消息在多个 Agent 间协作。

### Phase 1 — Hub MCP Server + list_agents（基础设施）

**改动范围**：
- `src/server/mod.rs`：新增 `/hub/mcp` 路由
- `src/hub/mcp.rs`（新模块）：MCP JSON-RPC 处理、`list_agents` 实现
- `src/hub/state.rs`：新增 `A2AState`

**验收**：Claude Code / Cursor 能通过 MCP 查询在线 Agent 列表。

### Phase 2 — call_agent 实现（核心能力）

**改动范围**：
- `src/hub/mcp.rs`：实现 `call_agent` handler
- `src/server/routes.rs`：`sendmessage` 中处理 `a2a_correlation_id`
- `src/hub/dispatch.rs`：`build_a2a_synthetic_message()`
- `HubExt`：新增 `a2a_correlation_id`、`a2a_caller` 字段
- `sdk/node/src/index.js`：新增 `ctx.agents.call/list`（可选，后续补）

**验收**：
- Agent A 调用 `call_agent('B', 'task')` → 微信显示 A→B 委派消息 → B 自动处理 → 微信显示 B 回复 → A 拿到结果继续 → 微信显示 A 最终回复
- 全程无需用户介入

---

## 7. 风险与控制

| 风险 | 缓解 |
|------|------|
| 死循环（A→B→A→...） | MCP 层记录调用深度，超过阈值（默认 5）直接报错返回 |
| call_agent 超时 | 120s 硬超时，超时后 MCP 返回错误，A 可降级处理 |
| 合成消息被 bot-echo 过滤误丢 | `from_user_id` 使用 `agent:{name}` 格式，不触发 bot-echo 跳过逻辑 |
| B 的 sendmessage 未携带 correlation_id | Bridge 需要完整回传 HubExt；在集成测试中覆盖 |
| 多个并发 call_agent 到同一 B | 每个调用有独立 correlation_id + session，天然隔离 |

---

## 8. 已确认的设计决策

| 决策点 | 结论 |
|--------|------|
| MCP Transport | **Streamable HTTP**（MCP 2025-11-05 标准，Claude Code / Cursor 均已支持） |
| Agent 简介来源 | **backend 注册时提供**：`/hub/register` 新增 `description` 字段 |
| 连锁委派 | 允许（B 也可以 `call_agent(C)`），用深度计数（默认上限 5）控制 |
| 微信展示格式 | A→B 委派消息用 A 的 footer，B 回复用 B 的 footer |

## 9. 后续开放问题

- `/hub/register` 的 `description` 字段是必填还是选填？（建议选填，默认为空）
- A2A session 是否需要落库持久化（重启恢复），还是内存 + TTL 清理？
