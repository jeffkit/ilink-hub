---
type: Reference
title: AgentProc 0.3 协议与 Bridge Profile
description: Bridge 与 profile 进程之间的 AgentProc 0.3 通信契约：NDJSON turn 输入、NDJSON 事件输出、流式与 permission 通道。
resource: docs/bridge/profile-spec.md
tags: [bridge, profile, protocol, agentproc, ndjson]
timestamp: 2026-07-13T21:30:00+08:00
---

# AgentProc 0.3 协议与 Bridge Profile

ilink-hub 的 Bridge 与 profile 进程之间采用 **AgentProc 0.3** 协议：以 **NDJSON**（Newline Delimited JSON）作为双向底层载体，**零 SDK 依赖**，跨平台。这是对旧版 0.2（环境变量输入 + sentinel 前缀 stdout）的**硬切换**，不再保留双协议兼容。

协议常量：`PROTOCOL_VERSION = "0.3"`（定义在 `src/bridge/protocol.rs`）。

## 输入：Bridge → Agent（stdin，单条 NDJSON turn）

Bridge 在启动 profile 进程后，向其 stdin 写入**一行** JSON（turn 对象），随后：

- `permission: false`（默认）：立即关闭 stdin；
- `permission: true`：保持 stdin 开启，用于后续写入 `permission_response` 帧（见下文「Permission 通道」）。

turn 对象结构：

```json
{
  "type": "turn",
  "message": "用户消息文本（路由后净文本，前缀已剥离）",
  "session_id": "Hub 持久化的后端 session UUID（空 = 新会话）",
  "from_user": "发送消息的用户 ID",
  "protocol_version": "0.3",
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
| `protocol_version` | 固定 `"0.3"` |
| `session_name` | session 可读名称 |
| `attachments` | 附件数组；`kind` ∈ {`image`, `file`, `video`}，`url` 为可下载地址，其余元数据可选 |
| `permission` | `true` = 开启 permission 通道（stdin 保持开启） |

> 环境变量仅用于**密钥与配置**（profile 的 `env` 块 + 基础设施注入），不再承载消息/session 等业务字段。
> 例外：`AGENT_CONTEXT_TOKEN` 仍由 Hub 作为 env 注入，用于回调凭证（ilink-hub 扩展，非 AgentProc 标准）。

## 输出：Agent → Bridge（stdout，NDJSON 事件流）

Agent 在 stdout 上逐行输出 NDJSON 事件，Bridge 从进程启动起**实时读取**。事件采用**封闭词汇表**：未知 `type` 静默忽略。

```json
{"type":"partial","text":"流式片段","role":"output"}
{"type":"text","text":"最终回复片段"}
{"type":"session","id":"<uuid>"}
{"type":"error","message":"可读错误文本"}
{"type":"permission_request","id":"req-42","tool":"Bash","input":{...}}
```

| 事件 | 用途 | Bridge 行为 |
|------|------|-------------|
| `partial` | 流式片段（`role`: `output`（默认）/ `thinking`） | `streaming` hint 为真时**立即**转发给用户；否则忽略 |
| `text` | 最终回复片段（可多次，Bridge 拼接） | 累积为最终 body；streaming 模式下若已有 partial 转发则去重丢弃（A1 dedup） |
| `session` | 上报 session id（last-wins） | 持久化为 Hub session ID |
| `error` | 终端错误 | 标记 turn 失败；streaming 模式经 partial 转发，非 streaming 进最终 body |
| `permission_request` | 工具授权请求（仅 `permission: true`） | 按 `permission_default` 策略回 `permission_response` |

### `partial` vs `text` 的分工

- `partial` = 流式增量，仅供 streaming 模式实时推送，**不**进入最终 body。
- `text` = 最终回复的权威内容，非 streaming 模式下作为唯一回复发出；streaming 模式下若 partial 已转发则被去重。

builtin agent 统一始终以 `stream-json` 运行底层 CLI 并**同时**发 `partial` + `text`，由 Bridge 依据 profile 的 `streaming` hint 决定取舍——agent 侧不再有 oneshot/streaming 分支。

## Exit code 优先级

turn 的成败按以下优先级判定（前者覆盖后者）：

1. **timeout** — 进程超时 → turn 失败
2. **error 事件** — 收到 `{"type":"error",...}` → turn 失败（错误文本已交付用户）
3. **进程 exit code** — 非零退出但已恢复 session/body 时容错；否则失败

## Permission 通道（可选）

profile 设置 `permission: true` 时开启：

1. Agent 在需要工具授权时发 `permission_request` 事件（含 `request_id` / `tool_name` / `input`）
2. Bridge 依据 `permission_default` 策略决定，并通过 stdin 写回一行 `permission_response` NDJSON：

```json
{"type":"permission_response","request_id":"req-42","behavior":"allow","message":"可选说明"}
```

3. Agent 收到响应后继续执行

### `permission_default` 策略

| 策略 | 行为 |
|------|------|
| `allow` | 自动批准所有工具调用（等价 `--dangerously-skip-permissions`，默认） |
| `deny` | 拒绝所有工具调用，agent 需在没有该工具的情况下继续 |
| `deny_logged` | 记录请求并拒绝（审计安全默认） |
| `ask` | **暂停 turn，经微信向用户提问**「🔧 工具 X 请求授权…回复『允许』或『拒绝』」，等用户下一条消息解析后回写 `permission_response` |

### `ask` 交互审批循环

`ask` 是完整的 WeChat 人机审批闭环：

1. Bridge executor 收到 `permission_request`，经 partial 通道向用户发出提问（工具名 + 输入预览）
2. 在 `ApprovalBroker` 上按 session-dispatch key 注册一个收件箱，turn 暂停
3. 用户在**同一 session** 的下一条消息被 `SessionDispatcher::dispatch` 优先投递到该收件箱（而非进入正常消息队列）
4. Bridge 宽松解析回复（`允许`/`yes`/`y`/`是`/`ok`/`1` → allow；`拒绝`/`no`/`n`/`否`/`0` → deny）；未识别则重新提示，**最多 2 次**后按拒绝处理
5. 超时（`permission_ask_timeout_secs`，默认 **600s / 10 分钟**）未回复 → 自动 deny 并提示用户「授权超时已拒绝」
6. 决策写回 agent stdin，agent 继续

> 审批窗口内用户的任何消息都视为审批回复（按上述规则解析）；不会并发触发新的 turn。

### 内置 `claude-code` 的 permission 模式

当 `type: claude-code` 的 profile 同时设置 `permission: true`，内置 agent 切换到 `claude --permission-prompt-tool stdio --permission-mode default`（替代 `--dangerously-skip-permissions`），并在 Claude 的 `control_request`(can_use_tool) ↔ AgentProc `permission_request`、`permission_response` ↔ Claude `control_response` 之间双向转译，从而把 Claude 的工具授权提示接到上面的 WeChat `ask` 闭环。多模态（image/file 附件）在 permission 模式下同样支持。

## Profile YAML 结构

### 单 Profile

```yaml
command: python3
args: [/path/to/my_ai.py]
cwd: /path/to/project
```

### 多 Profile（prefix 路由）

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/project
  cursor:
    type: cursor
    cwd: /path/to/project
routing: prefix    # 或 fixed
```

### 关闭流式（bridge-side hint）

```yaml
profiles:
  claude:
    type: claude-code
    streaming: false
```

`streaming` 是 **bridge 侧 hint**，决定是否转发 `partial`；agent 始终以 stream-json 运行。

### 0.3 新增字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `env_allowlist` | 无（全放行） | 限制 `env` 中变量展开的允许名单（未列出的未知变量展开为空，POSIX 语义） |
| `kill_grace_secs` | `default_kill_grace_secs` | 超时后给进程的优雅退出宽限秒数 |
| `permission` | `false` | 开启 permission 通道 |
| `permission_default` | `allow` | permission 默认策略：`allow` / `deny` / `deny_logged` / `ask`（`ask` 走 WeChat 交互审批） |
| `permission_ask_timeout_secs` | `600` | `ask` 策略等待用户回复的秒数；超时自动 deny |
| `truncation_suffix` | `"…(已截断)"` | 超长回复截断后缀 |
| `{{PROFILE_DIR}}` | — | 占位符，展开为 profile 目录 |

> 0.2 的 `stdin`（`StdinMode`）与 `cli_session_first_line_prefix` 字段已移除。

## 内置 Profile 类型

| `type` 值 | 说明 | 详细文档 |
|-----------|------|---------|
| `claude-code` | 包装 `claude` CLI | [Claude Code Bridge](claude-code.md) |
| `cursor` | 包装 Cursor `agent` CLI | [Cursor Bridge](cursor.md) |
| `codebuddy-code` | 包装 `codebuddy` CLI | — |
| `codex` | 包装 OpenAI `codex` CLI | — |
| `agy` | 包装 Google `agy` CLI | — |
| `recursive` | 包装 `recursive` agent CLI | [Recursive Bridge](recursive.md) |

## 自定义 Profile

任何可执行程序（Python、Node、shell 脚本等）都可以作为 profile，只需：

1. 从 stdin 读取一行 NDJSON `turn` 对象（`message` / `session_id` / `attachments` 等）
2. 在 stdout 逐行输出 NDJSON 事件（`partial` / `text` / `session` / `error`）

完整示例见 [`examples/`](../../examples/)。

## 从 0.2 迁移要点

- 输入：`AGENT_MESSAGE` / `AGENT_SESSION_ID` / `AGENT_STREAMING` 等 env → stdin NDJSON turn
- 输出：`AGENT_PARTIAL:` / `AGENT_SESSION:` / `AGENT_ERROR:` sentinel 行 → `{"type":"partial|session|error",...}` NDJSON 事件
- 附件：`AGENT_IMAGE_URL` / `AGENT_FILE_URL` env → turn 对象的 `attachments` 数组
- 最终回复：stdout 末尾的自由文本 → `{"type":"text",...}` 事件
- 流式：`AGENT_STREAMING` env 分支取消，agent 统一 stream-json，由 bridge hint 决定转发
