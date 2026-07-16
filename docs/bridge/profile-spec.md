# Bridge Profile 规范（AgentProc 0.4 + hub form YAML）

> 最后更新：2026-07-16  
> 知识库权威副本：[profile-protocol.md](../knowledge/bridges/profile-protocol.md)

iLink Hub Bridge 的 **profile** 是可执行脚本/程序：stdin 读一行 NDJSON turn → 处理 → stdout 逐行 NDJSON 事件。

协议常量：`PROTOCOL_VERSION = "0.4"`。

---

## 1. Wire 契约（摘要）

**stdin turn**：`type=turn`，字段含 `message` / `session_id` / `from_user` / `protocol_version=0.4` / `session_name` / `attachments` / `permission`。

**stdout 事件**：`partial` / `result` / `error` / `permission_request`（封闭词表；未知 type 忽略）。session 经事件字段 `session_id`（first non-empty wins）。

**permission**：`permission: true` 时 stdin 保持开启；Bridge 对 `permission_request` **恒 allow**。

完整字段表与退出码优先级见 [profile-protocol.md](../knowledge/bridges/profile-protocol.md)。

---

## 2. YAML hub form（一文件一 profile）

```yaml
description: optional for Hub MCP list_agents
script: ./my_handler.py          # 可选；agentproc.command 优先
agentproc:
  executor: claude-code          # 或 command/args
  cwd: /path/to/project
  streaming: true
  permission: false
  send_error_reply: true
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

| 已移除 | 替代 |
|--------|------|
| `profiles` + `routing` | manager 多文件 + Hub `/use` |
| `type:` | `agentproc.executor` |
| `skip_bot_messages` / `require_text` | 内化恒 true |
| `permission_default` / WeChat ask | permission 恒 allow |

`script:` 扩展名推断：`.py`→python3，`.js`→node，`.ts`→npx tsx，`.sh`→bash，`.rb`→ruby。

---

## 3. 用 SDK 写 profile（推荐）

```bash
pip install "agentproc>=0.9"
# 或 npm install agentproc@^0.9
```

```python
from agentproc import create_profile

async def handler(ctx):
    reply = await my_ai_call(ctx.message)
    return reply

create_profile(handler)
```

```yaml
description: my SDK bot
script: ./my_handler.py
agentproc:
  timeout_secs: 60
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

Node：

```js
const { createProfile } = require('agentproc');
createProfile(async ({ message }) => ({ response: await myAICall(message) }));
```

开发指南：[develop-python.md](./develop-python.md) / [develop-nodejs.md](./develop-nodejs.md)。

---

## 4. 内置 executor

`claude-code` / `cursor` / `codex` / `codebuddy` / `agy` / `recursive` / `opencode` — 见 [overview](../knowledge/bridges/overview.md)。

---

## 5. 自测

```bash
TURN='{"type":"turn","message":"你好","session_id":"","from_user":"test","protocol_version":"0.4","session_name":"default","attachments":[],"permission":false}'
echo "$TURN" | ilink-hub-bridge profile claude-code
```

期望 stdout 含 `partial` 与/或带 `session_id` 的 `result`。
