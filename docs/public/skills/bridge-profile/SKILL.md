---
name: bridge-profile
description: >-
  This skill should be used when the user wants to create, edit, test, or publish
  an ilink-hub-bridge profile YAML, or develop a custom profile handler using the
  Python or Node.js SDK. Triggers on: "创建 bridge profile", "新增 bridge", "添加 profile",
  "写一个 bridge", "发布 profile", "测试 profile", "bridge 配置", "用 Python/JS 写 profile",
  "自定义 profile", "ilink bridge profile", "create bridge profile", "add new bridge",
  "publish profile", "develop profile with SDK".
version: 0.2.0
source: https://jeffkit.github.io/ilink-hub/skills/bridge-profile/SKILL.md
---

# ilink-hub-bridge Profile 开发 Skill

本 skill 覆盖 bridge profile 的完整生命周期：**需求确认 → YAML 创建 → 测试 → 发布**，以及用 Python / Node.js SDK 开发自定义 handler 的完整流程。

---

## 概念速查

| 术语 | 说明 |
|------|------|
| **Profile YAML** | 描述 bridge 行为的配置文件，放在 `~/.ilink-hub-bridge/profiles/` 由 manager 自动发现 |
| **AgentProc 0.4** | bridge 与 handler 间的通信协议：stdin 写一行 NDJSON turn，stdout 逐行输出 NDJSON 事件 |
| **executor: claude-code** | 内置 executor，自动处理 Claude Code CLI 的 `--resume` 会话续接 |
| **agentproc:** | YAML 中嵌套的 agentproc 规范执行块（一文件一 profile） |
| **script:** | 指定脚本路径，bridge 按扩展名自动推断运行时（.py/.js/.ts/.sh/.rb） |
| **SDK** | `agentproc`（Python/Node.js），封装 0.4 NDJSON 协议样板代码 |

---

## Step 1：确认场景

向用户确认（未明确时询问）：

1. **Profile 名称**（文件名 stem，如 `my-claude` → `my-claude.yaml`）
2. **用途**：接 Claude Code、自定义脚本/SDK，还是其他 CLI（Cursor、Codex）？
3. **项目目录** `cwd`：在哪个目录下执行？
4. **多后端**：需要多个 CLI 时用 manager 多文件 + Hub `/use`（不再支持单文件 prefix 路由）
5. **特殊 env**：API Key、BASE_URL、模型名等？**绝对不要把明文 key 写进 YAML**——见 [Secrets & 环境变量](#secrets--env-vars)。

**快速路由：**
- 接 Claude Code → [内置 claude-code YAML](#yaml-claude-code)
- 接自定义 Python/JS 逻辑 → [SDK Handler 开发](#sdk-development)
- 接其他 CLI（Cursor/Codex）→ [自定义 command YAML](#yaml-custom-cli)
- 多后端 → manager 目录多文件 + `/use`（见 [profile-protocol](../../../knowledge/bridges/profile-protocol.md)）

---

## Step 2：生成 Profile YAML

### 发布路径

`~/.ilink-hub-bridge/profiles/<name>.yaml`

若用户有项目源码目录，可先在项目内创建草稿再发布；否则直接写到发布路径。

---

### 内置 claude-code（推荐） {#yaml-claude-code}

```yaml
# ~/.ilink-hub-bridge/profiles/<name>.yaml
description: Claude Code on project
agentproc:
  executor: claude-code
  cwd: /path/to/project
  # env:
  #   CLAUDE_MODEL: sonnet
  #   ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

---

### 自定义 CLI（Cursor / Codex / 任意命令） {#yaml-custom-cli}

```yaml
description: custom CLI
agentproc:
  command: your-cli-command
  args: ["-p", "{{MESSAGE}}"]
  cwd: /path/to/project
  timeout_secs: 300
  max_reply_chars: 8000
  send_error_reply: true
```

> 自定义 CLI 默认通过 stdin 收到一行 NDJSON turn；若 CLI 需要消息作为命令行参数，用 `{{MESSAGE}}` 占位符。stdout 应输出 `partial` / `result` / `error`（0.4）。

---

### 自定义 SDK 脚本（Python/JS） {#yaml-script}

```yaml
description: custom SDK handler
script: /path/to/handler.py   # bridge 自动调用 python3 handler.py
# 或 script: /path/to/handler.js
agentproc:
  timeout_secs: 60
  max_reply_chars: 8000
  send_error_reply: true
  env:
    MY_API_KEY: ${MY_API_KEY}
```

**`script:` 运行时推断规则：**

| 扩展名 | 推断运行时 |
|--------|-----------|
| `.py` | `python3 <script>` |
| `.js` / `.mjs` | `node <script>` |
| `.ts` | `npx tsx <script>` |
| `.sh` / `.bash` | `bash <script>` |
| `.rb` | `ruby <script>` |
| 其他 | 直接执行（需 chmod +x） |

---

### YAML 字段速查

| 字段 | 说明 |
|------|------|
| `description` | 文件级：Agent 描述 |
| `script` | 文件级：脚本路径，自动推断运行时；`agentproc.command` 优先 |
| `agentproc.executor` | `claude-code` / `cursor` / `codex` / `codebuddy` / `agy` / `recursive` / `opencode` |
| `agentproc.command` / `args` | 自定义命令与参数；支持 `{{MESSAGE}}` 等占位符 |
| `agentproc.cwd` | 工作目录，支持 `~` |
| `agentproc.env` | 密钥/配置；`${VAR}` 从 bridge 进程 env 展开 |
| `agentproc.env_allowlist` | 限制可展开变量 |
| `agentproc.timeout_secs` | 超时（默认 1800） |
| `agentproc.kill_grace_secs` | 优雅退出宽限 |
| `agentproc.max_reply_chars` | 截断上限（默认 8000） |
| `agentproc.streaming` | 是否转发 `partial`（默认 true） |
| `agentproc.permission` | permission 通道（默认 false；请求恒 allow） |
| `agentproc.send_error_reply` | 错误是否回用户（默认 true） |
| `agentproc.include_stderr_in_reply` | 是否附加 stderr（默认 false） |

---

## Step 3：用 SDK 开发自定义 Handler {#sdk-development}

当内置类型不满足需求，需要调用 LLM API、自定义逻辑、多轮对话时，使用 SDK 编写 handler。

### AgentProc 0.4 协议说明

bridge 向 handler 的 stdin 写一行 NDJSON turn 对象，handler 在 stdout 逐行输出 NDJSON 事件：

**stdin turn 对象：**

```json
{"type":"turn","message":"用户消息","session_id":"<uuid 或空>","from_user":"<user>","protocol_version":"0.4","session_name":"default","attachments":[...],"permission":false}
```

| turn 字段 | 说明 |
|-----------|------|
| `message` | 用户消息文本 |
| `session_id` | Hub 持久化的 session UUID（空=新会话） |
| `session_name` | session 可读名称（默认 `default`） |
| `from_user` | 发送消息的用户 ID |
| `attachments` | 附件数组（`kind` / `url` / 元数据） |

**stdout NDJSON 事件：**

```
{"type":"partial","text":"流式片段"}        ← 实时分块（streaming hint 为真时转发）
{"type":"text","text":"最终回复片段"}        ← 最终回复（可多次，bridge 拼接）
{"type":"session","id":"<uuid>"}            ← 上报 session id
{"type":"error","message":"可读错误文本"}    ← 终端错误
```

> SDK（`agentproc`）封装了读写 NDJSON 的样板，handler 只需返回字符串或 `AgentResult`。

---

### Python SDK

```bash
mkdir my-profile && cd my-profile
python3 -m venv .venv && source .venv/bin/activate
pip install agentproc
```

**最简 handler（`handler.py`）：**

```python
from agentproc import create_profile, AgentContext

async def handler(ctx: AgentContext) -> str:
    return f"你说的是：{ctx.message}"

create_profile(handler)
```

**接 OpenAI/Claude API（`handler.py`）：**

```python
import os
import anthropic
from agentproc import create_profile, AgentContext

client = anthropic.AsyncAnthropic(api_key=os.environ["ANTHROPIC_API_KEY"])

async def handler(ctx: AgentContext) -> str:
    response = await client.messages.create(
        model="claude-opus-4-5",
        max_tokens=2048,
        messages=[{"role": "user", "content": ctx.message}],
    )
    return response.content[0].text

create_profile(handler)
```

**多轮对话（`handler.py`）：**

```python
import os
from openai import AsyncOpenAI
from agentproc import (
    create_profile, AgentContext, AgentResult,
    load_history, append_history, HistoryEntry,
)

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: AgentContext) -> AgentResult:
    history = load_history(ctx.session_id)
    messages = [
        {"role": "system", "content": "你是一个友好的 AI 助手。"},
        *[{"role": e.role, "content": e.content} for e in history],
        {"role": "user", "content": ctx.message},
    ]
    completion = await client.chat.completions.create(
        model="gpt-4o-mini", messages=messages
    )
    reply = completion.choices[0].message.content

    append_history(ctx.session_id, [
        HistoryEntry(role="user", content=ctx.message),
        HistoryEntry(role="assistant", content=reply),
    ])
    return AgentResult(response=reply, session_id=ctx.session_id)

create_profile(handler)
```

对应 YAML：
```yaml
description: my agent
script: /path/to/handler.py
agentproc:
  timeout_secs: 60
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

---

### Node.js SDK

```bash
mkdir my-profile && cd my-profile
npm init -y
npm install agentproc
```

**最简 handler（`handler.js`）：**

```js
const { createProfile } = require('agentproc');

createProfile(async ({ message }) => {
  return `你说的是：${message}`;
});
```

**接 OpenAI/Claude API（`handler.js`）：**

```js
const { createProfile } = require('agentproc');
const Anthropic = require('@anthropic-ai/sdk');

const client = new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY });

createProfile(async ({ message }) => {
  const response = await client.messages.create({
    model: 'claude-opus-4-5',
    max_tokens: 2048,
    messages: [{ role: 'user', content: message }],
  });
  return response.content[0].text;
});
```

**多轮对话（`handler.js`）：**

```js
const { createProfile, loadHistory, appendHistory } = require('agentproc');
const OpenAI = require('openai');

const client = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });

createProfile(async ({ message, sessionId }) => {
  const history = loadHistory(sessionId);
  const messages = [
    { role: 'system', content: '你是一个友好的 AI 助手。' },
    ...history.map(e => ({ role: e.role, content: e.content })),
    { role: 'user', content: message },
  ];
  const completion = await client.chat.completions.create({
    model: 'gpt-4o-mini', messages,
  });
  const reply = completion.choices[0].message.content;

  appendHistory(sessionId, [
    { role: 'user', content: message },
    { role: 'assistant', content: reply },
  ]);
  return { response: reply, sessionId };
});
```

对应 YAML：
```yaml
description: my agent
script: /path/to/handler.js
agentproc:
  timeout_secs: 60
  env:
    OPENAI_API_KEY: ${OPENAI_API_KEY}
```

---

## Step 4：测试

不启动完整 bridge，向 stdin 写一行 turn NDJSON 模拟调用：

```bash
# 公共 turn 模板
TURN='{"type":"turn","message":"你好","session_id":"","from_user":"test","protocol_version":"0.4","session_name":"default","attachments":[]}'

# 测试内置 claude-code
echo "$TURN" | ilink-hub-bridge profile claude-code

# 测试自定义 Python handler
echo "$TURN" | python3 /path/to/handler.py

# 测试自定义 JS handler
echo "$TURN" | node /path/to/handler.js
```

**验证输出（NDJSON 事件流）：**
- 最终回复：`{"type":"result","text":...,"session_id":"..."}`
- 流式：另有 `partial` 事件；session 经事件字段 `session_id`
- 退出码 0 = 成功；`{"type":"error",...}` 事件 = turn 失败

---

## Step 5：发布

```bash
mkdir -p ~/.ilink-hub-bridge/profiles

# 直接写入或复制
cp /path/to/draft.yaml ~/.ilink-hub-bridge/profiles/<name>.yaml
```

manager 每 5 秒扫描一次 profiles 目录，自动发现新文件并启动子进程。

如果 manager 未运行：
```bash
ilink-hub-bridge manager
```

**验证 manager 识别：** 日志中出现
```
INFO ilink_hub::bridge::manager: starting child bridge profile=<name> ...
```

---

## Step 6：微信中使用

```
/list              # 查看所有已注册 bridge
/use local-<hostname>-<name>   # 切换到此 bridge
```

客户端名格式：`local-<hostname>-<profile-stem>`
（如 `my-claude.yaml` → `local-MacBook-my-claude`）

---

## Secrets & 环境变量 {#secrets--env-vars}

**铁律：永远不要把明文 API key / token / password 写进 profile YAML。**

`env:` 字段里的 `${VAR_NAME}` 会在加载时按 POSIX 语义从 bridge 进程环境变量展开（**未知变量展开为空字符串**，不报错）。若 profile 设置了 `env_allowlist`，则只有名单内的变量允许展开，其余一律展开为空。

### 推荐做法

```yaml
description: my agent
script: /path/to/handler.py
agentproc:
  timeout_secs: 60
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

启动 manager 之前先 export：

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
ilink-hub-bridge manager
```

或一行启动：

```bash
ANTHROPIC_API_KEY="sk-ant-..." ilink-hub-bridge manager
```

如果 manager 是 launchd / 守护进程启动的，进程 env 不一定有这些变量——那种场景下需要把
`export` 写到 `~/.zshenv` / `~/.bash_profile`，或用专门的 env 加载器。

### env_allowlist（可选）

限制可展开的变量，避免意外把敏感环境变量透传给子进程：

```yaml
description: my agent
script: /path/to/handler.py
agentproc:
  env_allowlist: [ANTHROPIC_API_KEY, OPENAI_API_KEY]
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

### permission 通道（可选）

`agentproc.permission: true` 开启工具授权通道：handler 发 `permission_request`，bridge **恒 allow** 并回写 `permission_response`。WeChat ask / `permission_default` 已移除。

```yaml
description: Claude with permission channel
agentproc:
  executor: claude-code
  permission: true
```

---

## 调试速查

```bash
# 开启消息 dump（查看完整 WeixinMessage JSON）
ILINKHUB_BRIDGE_DUMP_MSG=1 ilink-hub-bridge manager

# 查看已发布 profiles
ls ~/.ilink-hub-bridge/profiles/

# 查看 manager 凭证（每个 profile 独立）
ls ~/.ilink-hub-bridge/credentials/

# 强制重置某 profile 的凭证（token 失效时）
rm ~/.ilink-hub-bridge/credentials/<name>-credentials.json
```
