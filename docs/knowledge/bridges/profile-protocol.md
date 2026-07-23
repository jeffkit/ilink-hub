---
type: Reference
title: AgentProc 0.4 协议与 Bridge Profile
description: Bridge 与 profile 进程之间的 AgentProc 0.4 通信契约：NDJSON turn 输入、NDJSON 事件输出；YAML 采用 agentproc hub form（agentproc: 嵌套）。
resource: https://github.com/jeffkit/im-agentproc
tags: [bridge, profile, protocol, agentproc, ndjson]
timestamp: 2026-07-16T14:30:00+08:00
---

# AgentProc 0.4 协议与 Bridge Profile

> **仓库迁移（2026-07-20）**：Bridge 代码已从 `ilink-hub` 的 `src/bridge/` 物理拆分到独立仓库 [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)（crate `im-agentproc`，bin `im-agentproc`）。本文档仍描述 Bridge 协议（概念不变），代码与 profile 规范以 im-agentproc 为准。详见 `docs/proposals/bridge-as-multi-im-runtime.md` 附录 A。

ilink-hub 的 Bridge 与 profile 进程之间采用 **AgentProc 0.4** 协议：以 **NDJSON**（Newline Delimited JSON）作为双向底层载体，**零 SDK 依赖**，跨平台。

协议常量：`PROTOCOL_VERSION = "0.4"`（原定义在 `src/bridge/protocol.rs`，现已随 bridge 迁入 im-agentproc）。

**YAML 形态**：一个文件 = 一个 Hub 客户端 = 一个 agentproc profile。执行配置嵌在 `agentproc:` 下（与 agentproc 规范字段对齐）；`description` / `script` 作为文件级 sibling。不再支持 `profiles:` 多 profile 映射、`routing`、`permission_default` / WeChat ask 审批。

## 输入：Bridge → Agent（stdin，单条 NDJSON turn）

Bridge 在启动 profile 进程后，向其 stdin 写入**一行** JSON（turn 对象），随后：

- `permission: false`（默认）：立即关闭 stdin；
- `permission: true`：保持 stdin 开启，用于后续写入 `permission_response` 帧（见下文「Permission 通道」）。

```json
{
  "type": "turn",
  "message": "用户消息文本",
  "session_id": "Hub 持久化的后端 session UUID（空 = 新会话）",
  "from_user": "发送消息的用户 ID",
  "protocol_version": "0.4",
  "session_name": "session 可读名称（默认 default）",
  "attachments": [
    {"kind": "image", "url": "https://...", "filename": "a.png", "mime_type": "image/png", "size": 12345},
    {"kind": "file",  "url": "https://...", "filename": "b.pdf", "mime_type": "application/pdf"}
  ],
  "permission": false
}
```

| 字段 | 说明 |
|------|------|
| `type` | 固定 `"turn"` |
| `message` | 用户消息文本 |
| `session_id` | 后端 session UUID（空 = 新会话） |
| `from_user` | 发送用户 ID |
| `protocol_version` | 固定 `"0.4"` |
| `session_name` | session 可读名称 |
| `attachments` | 附件数组；`kind` ∈ {`image`, `file`, `video`} |
| `permission` | `true` = 开启 permission 通道（stdin 保持开启） |

> 环境变量仅用于**密钥与配置**（`agentproc.env` + 基础设施注入）。例外：`AGENT_CONTEXT_TOKEN` 仍由 Hub 作为 env 注入（ilink-hub 扩展）。

## 输出：Agent → Bridge（stdout，NDJSON 事件流）

```json
{"type":"partial","text":"流式片段","role":"output","session_id":"<uuid>"}
{"type":"result","text":"最终回复","session_id":"<uuid>","usage":{"input_tokens":12,"output_tokens":34}}
{"type":"error","message":"可读错误文本","session_id":"<uuid>"}
{"type":"permission_request","request_id":"req-42","tool_name":"Bash","input":{...},"session_id":"<uuid>"}
```

| 事件 | 用途 | Bridge 行为 |
|------|------|-------------|
| `partial` | 流式片段（`role`: `output` / `thinking`） | `streaming` 为真时立即转发给用户；否则忽略 |
| `result` | 终端成功正文（每 turn 至多一条） | 非 streaming 下作为最终 body；streaming 下若已有 partial 则去重丢弃 |
| `error` | 终端错误 | 标记 turn 失败；是否回用户由 `send_error_reply` 控制 |
| `permission_request` | 工具授权请求（仅 `permission: true`） | **恒 allow**（写回 `permission_response`）；不再有 per-profile 策略 / WeChat ask |

### `session_id`（事件字段）

- 可选，可出现在 `partial` / `result` / `error` / `permission_request` 上。
- Bridge **first non-empty wins**；后续冲突值告警并忽略。
- 0.3 的 `{"type":"session"}` / `{"type":"text"}` 在 0.4 中为未知类型，**静默忽略**。

## Exit code 优先级

1. **timeout** — 进程超时 → turn 失败  
2. **error 事件** — 收到 `{"type":"error",...}` → turn 失败  
3. **进程 exit code** — 非零退出但已恢复 session/body 时容错；否则失败  

## Permission 通道（可选）

`agentproc.permission: true` 时开启：

1. Agent 发 `permission_request`（含 `request_id` / `tool_name` / `input`）
2. Bridge **一律 allow**，经 stdin 写回：

```json
{"type":"permission_response","request_id":"req-42","behavior":"allow","message":"可选说明"}
```

3. Agent 继续执行

> 已移除：`permission_default`、`permission_ask_timeout_secs`、`ApprovalBroker` / 微信交互审批。后续若再接入，会走 agentproc permission 事件重新设计。

当 `executor: claude-code` 且 `permission: true` 时，内置 agent 使用 `claude --permission-prompt-tool stdio --permission-mode default`，在 Claude `control_request` ↔ AgentProc `permission_request` 之间转译；当前 Bridge 侧仍恒 allow。

## Profile YAML 结构（hub form）

```yaml
description: optional text for Hub MCP list_agents
script: ./handler.py          # 可选 shorthand；显式 agentproc.command 优先
agentproc:
  executor: claude-code       # 或 cursor / codex / codebuddy / agy / recursive / opencode …
  # command / args:           # 无 executor 或自定义 spawn 时使用
  cwd: /path/to/project
  streaming: true
  permission: false
  send_error_reply: true      # agentproc 规范字段；ilink-hub 不在更高层覆盖
  env:
    ANTHROPIC_API_KEY: ${MINIMAX_API_KEY}
    CLAUDE_MODEL: MiniMax-M2.5
```

### 关闭流式

```yaml
description: claude oneshot reply
agentproc:
  executor: claude-code
  cwd: /path/to/project
  streaming: false
```

### `agentproc:` 字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `executor` | 无 | 进程内 executor 名（如 `claude-code`）；识别则走 agentproc SDK，否则 spawn `command`/`args` |
| `command` / `args` | 空 | 自定义可执行文件与参数 |
| `cwd` | 进程 cwd | 工作目录；相对路径相对 `{{PROFILE_DIR}}` |
| `env` | `{}` | 密钥/配置；`${VAR}` 从 bridge 进程 env 展开 |
| `env_allowlist` | 无（全放行） | 限制可展开的变量名 |
| `timeout_secs` | `1800` | 主操作超时（秒） |
| `kill_grace_secs` | `5` | SIGTERM→SIGKILL 宽限 |
| `max_reply_chars` | `8000` | 回复截断上限 |
| `truncation_suffix` | `…(输出已截断)` | 截断后缀 |
| `include_stderr_in_reply` | `false` | 成功时是否拼 stderr |
| `send_error_reply` | `true` | CLI/`error` 是否回用户（规范字段） |
| `streaming` | `true` | bridge 侧是否转发 `partial` |
| `permission` | `false` | 开启 permission 通道 |

### 文件级 sibling

| 字段 | 说明 |
|------|------|
| `description` | Agent 描述（Hub MCP `list_agents`） |
| `script` | 按扩展名推断 runtime（`.py`→python3 等）；`agentproc.command` 优先 |

### 已移除（不再解析）

| 旧字段 | 替代 |
|--------|------|
| `profiles:` + `routing` | 一文件一 profile；多后端用 manager 多文件 + Hub `/use` |
| `type:` | `agentproc.executor` |
| `skip_bot_messages` / `require_text` | 内化为恒 `true`（跳过 bot 消息；无文本不触发） |
| 顶层 `send_error_reply` | 仅 `agentproc.send_error_reply` |
| `permission_default` / `permission_ask_timeout_secs` | permission 恒 allow |

## 内置 Executor

| `executor` 值 | 说明 | 详细文档 |
|---------------|------|---------|
| `claude-code` | 包装 `claude` CLI | [Claude Code Bridge](claude-code.md) |
| `cursor` | 包装 Cursor `agent` CLI | [Cursor Bridge](cursor.md) |
| `codebuddy` | 包装 `codebuddy` CLI | — |
| `codex` | 包装 OpenAI `codex` CLI | — |
| `agy` | 包装 Google `agy` CLI | — |
| `recursive` | 包装 `recursive` agent CLI | [Recursive Bridge](recursive.md) |
| `opencode` | 包装 OpenCode CLI | — |

## 自定义 Profile

任何可执行程序都可作 profile，只需：

1. 从 stdin 读一行 NDJSON `turn`
2. 在 stdout 逐行输出 NDJSON 事件（`partial` / `result` / `error`；可选 `session_id`）

推荐：`pip install "agentproc>=0.9"` / `npm install agentproc@^0.9`。

## 固定入站过滤（非配置项）

- 始终跳过 bot 侧消息（避免回路）
- 始终要求文本正文（纯图片/语音等不触发 CLI）

## 从旧 YAML 迁移

1. 去掉 `profiles:` / `routing:` / `skip_bot_messages` / `require_text` / 顶层 `send_error_reply`
2. 原 profile 字段迁入 `agentproc:`；`type:` → `executor:`
3. `codebuddy-code` → `codebuddy`
4. 多 profile 拆成多个 YAML 文件（manager 目录下一文件一客户端）
5. 删除 `permission_default` / `permission_ask_timeout_secs`（permission 恒 allow）

## 从 0.3 迁移要点（wire）

- 删除 `{"type":"session"}` / `{"type":"text"}`；最终正文改为单条 `{"type":"result"}`
- session 连续性改为事件字段 `session_id`（Bridge first-wins）
- `protocol_version` 改为 `"0.4"`
