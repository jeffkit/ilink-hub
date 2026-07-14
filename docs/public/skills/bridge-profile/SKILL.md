---
name: bridge-profile
description: >-
  This skill should be used when the user wants to create, edit, test, or publish
  an ilink-hub-bridge profile YAML, or develop a custom profile handler using the
  Python or Node.js SDK. Triggers on: "创建 bridge profile", "新增 bridge", "添加 profile",
  "写一个 bridge", "发布 profile", "测试 profile", "bridge 配置", "用 Python/JS 写 profile",
  "自定义 profile", "ilink bridge profile", "create bridge profile", "add new bridge",
  "publish profile", "develop profile with SDK".
version: 0.1.0
source: https://jeffkit.github.io/ilink-hub/skills/bridge-profile/SKILL.md
---

# ilink-hub-bridge Profile 开发 Skill

本 skill 覆盖 bridge profile 的完整生命周期：**需求确认 → YAML 创建 → 测试 → 发布**，以及用 Python / Node.js SDK 开发自定义 handler 的完整流程。

---

## 概念速查

| 术语 | 说明 |
|------|------|
| **Profile YAML** | 描述 bridge 行为的配置文件，放在 `~/.ilink-hub-bridge/profiles/` 由 manager 自动发现 |
| **AgentProc 0.3** | bridge 与 handler 间的通信协议：stdin 写一行 NDJSON turn，stdout 逐行输出 NDJSON 事件 |
| **type: claude-code** | 内置类型，自动处理 Claude Code CLI 的 `--resume` 会话续接 |
| **script:** | 指定脚本路径，bridge 按扩展名自动推断运行时（.py/.js/.ts/.sh/.rb） |
| **SDK** | `agentproc`（Python/Node.js），封装 0.3 NDJSON 协议样板代码 |

---

## Step 1：确认场景

向用户确认（未明确时询问）：

1. **Profile 名称**（文件名 stem，如 `my-claude` → `my-claude.yaml`）
2. **用途**：接 Claude Code、自定义脚本/SDK，还是其他 CLI（Cursor、Codex）？
3. **项目目录** `cwd`：在哪个目录下执行？
4. **路由**：是否需要前缀路由（`/new`、`/ask` 等不同 profile）？
5. **特殊 env**：API Key、BASE_URL、模型名等？**绝对不要把明文 key 写进 YAML**——见 [Secrets & 环境变量](#secrets--env-vars)。

**快速路由：**
- 接 Claude Code → [内置 claude-code YAML](#yaml-claude-code)
- 接自定义 Python/JS 逻辑 → [SDK Handler 开发](#sdk-development)
- 接其他 CLI（Cursor/Codex）→ [自定义 command YAML](#yaml-custom-cli)
- 多前缀路由 → [prefix 路由模板](#yaml-prefix)

---

## Step 2：生成 Profile YAML

### 发布路径

`~/.ilink-hub-bridge/profiles/<name>.yaml`

若用户有项目源码目录，可先在项目内创建草稿再发布；否则直接写到发布路径。

---

### 内置 claude-code（推荐） {#yaml-claude-code}

```yaml
# ~/.ilink-hub-bridge/profiles/<name>.yaml
profiles:
  claude:
    type: claude-code       # 内置：自动管理 --resume 续接上下文
    cwd: /path/to/project   # ← 改为实际项目目录
    # 可选：
    # env:
    #   ILINK_CLAUDE_MODEL: sonnet
    #   ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}    # 引用进程 env；不要写明文 key

routing:
  strategy: fixed
  default_profile: claude

skip_bot_messages: true
require_text: true
send_error_reply: true
```

---

### 带前缀路由的 claude-code {#yaml-prefix}

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/project
    env:
      ILINK_CLAUDE_MODEL: sonnet

  claude_new:
    type: claude-code
    cwd: /path/to/project
    # /new 是 Hub 级命令（强制新会话），无需在此设 env；
    # 这里仅演示前缀路由到独立 profile。

routing:
  strategy: prefix
  default_profile: claude
  prefix_rules:
    - prefix: "/ask "       # "/ask 帮我写函数" → MESSAGE = "帮我写函数"
      profile: claude_new

skip_bot_messages: true
require_text: true
send_error_reply: true
```

---

### 自定义 CLI（Cursor / Codex / 任意命令） {#yaml-custom-cli}

```yaml
profiles:
  my-cli:
    command: your-cli-command
    args: ["-p", "{{MESSAGE}}"]
    cwd: /path/to/project
    timeout_secs: 300
    max_reply_chars: 8000

routing:
  strategy: fixed
  default_profile: my-cli

skip_bot_messages: true
require_text: true
send_error_reply: true
```

> 自定义 CLI 默认通过 stdin 收到一行 NDJSON turn；若 CLI 需要消息作为命令行参数，用 `{{MESSAGE}}` 占位符（`{{SESSION_ID}}` / `{{SESSION_NAME}}` / `{{PROFILE_DIR}}` 同样可用）。CLI 的 stdout 应逐行输出 NDJSON 事件（`partial` / `text` / `session` / `error`）；若只输出纯文本，bridge 会作为最终回复处理。

---

### 自定义 SDK 脚本（Python/JS） {#yaml-script}

```yaml
profiles:
  my-bot:
    script: /path/to/handler.py   # bridge 自动调用 python3 handler.py
    # 或 script: /path/to/handler.js  → node handler.js
    timeout_secs: 60
    max_reply_chars: 8000
    env:
      MY_API_KEY: ${MY_API_KEY}    # 引用进程 env；不要写明文 key

routing:
  strategy: fixed
  default_profile: my-bot

skip_bot_messages: true
require_text: true
send_error_reply: true
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
| `type` | 内置类型：`claude-code` / `cursor` / `codex` / `codebuddy-code` / `agy` / `recursive` |
| `script` | 脚本路径，自动推断运行时 |
| `command` | 显式命令（优先级高于 script/type） |
| `args` | 支持占位符 `{{MESSAGE}}` / `{{SESSION_ID}}` / `{{SESSION_NAME}}` / `{{PROFILE_DIR}}` |
| `cwd` | 工作目录，支持 `~` 展开 |
| `env` | 注入的环境变量（仅密钥/配置；业务字段走 stdin turn） |
| `env_allowlist` | 限制 `env` 变量展开的允许名单（未列出的未知变量展开为空，POSIX 语义） |
| `timeout_secs` | 超时（默认 60） |
| `kill_grace_secs` | 超时后优雅退出宽限秒数 |
| `max_reply_chars` | 回复最大字符数（默认 4000） |
| `truncation_suffix` | 超长回复截断后缀（默认 `…(已截断)`） |
| `streaming` | bridge 侧 hint：是否转发 `partial` 事件（默认 true） |
| `permission` | 开启 permission 通道（默认 false） |
| `permission_default` | permission 默认策略：`allow` / `deny` / `deny_logged` / `ask`（默认 allow；`ask` 走微信交互审批） |
| `permission_ask_timeout_secs` | `ask` 策略等待用户回复秒数（默认 600）；超时自动 deny |
| `include_stderr_in_reply` | 是否附加 stderr（默认 false） |

---

## Step 3：用 SDK 开发自定义 Handler {#sdk-development}

当内置类型不满足需求，需要调用 LLM API、自定义逻辑、多轮对话时，使用 SDK 编写 handler。

### AgentProc 0.3 协议说明

bridge 向 handler 的 stdin 写一行 NDJSON turn 对象，handler 在 stdout 逐行输出 NDJSON 事件：

**stdin turn 对象：**

```json
{"type":"turn","message":"用户消息","session_id":"<uuid 或空>","from_user":"<user>","protocol_version":"0.3","session_name":"default","attachments":[...],"permission":false}
```

| turn 字段 | 说明 |
|-----------|------|
| `message` | 用户消息文本（前缀路由后的净文本） |
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
profiles:
  my-bot:
    script: /path/to/handler.py
    timeout_secs: 60
    env:
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}    # 引用进程 env
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
profiles:
  my-bot:
    script: /path/to/handler.js
    timeout_secs: 60
    env:
      OPENAI_API_KEY: ${OPENAI_API_KEY}    # 引用进程 env
```

---

## Step 4：测试

不启动完整 bridge，向 stdin 写一行 turn NDJSON 模拟调用：

```bash
# 公共 turn 模板
TURN='{"type":"turn","message":"你好","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}'

# 测试内置 claude-code
echo "$TURN" | ilink-hub-bridge profile claude-code

# 测试自定义 Python handler
echo "$TURN" | python3 /path/to/handler.py

# 测试自定义 JS handler
echo "$TURN" | node /path/to/handler.js
```

**验证输出（NDJSON 事件流）：**
- 若管理 session，应有 `{"type":"session","id":"<uuid>"}`
- 回复文本通过 `{"type":"text","text":...}` 事件输出（流式时另有 `partial` 事件）
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
profiles:
  my-bot:
    script: /path/to/handler.py
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
profiles:
  my-bot:
    script: /path/to/handler.py
    env_allowlist: [ANTHROPIC_API_KEY, OPENAI_API_KEY]
    env:
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
```

### permission 通道与 `ask` 审批（可选）

`permission: true` 开启工具授权通道：handler 发 `{"type":"permission_request",...}` 事件，bridge 按 `permission_default` 决策并经 stdin 回写 `{"type":"permission_response",...}`。`permission_default: ask` 会暂停 turn、经微信向用户提问「🔧 工具 X 请求授权…回复『允许』或『拒绝』」，用户在**同一 session** 的下一条消息被解析为审批回复（`允许`/`yes`/`1` → allow，`拒绝`/`no`/`0` → deny，未识别重提示最多 2 次后拒绝），超时（`permission_ask_timeout_secs`，默认 600s）自动拒绝。

```yaml
profiles:
  claude:
    type: claude-code
    permission: true
    permission_default: ask
    permission_ask_timeout_secs: 600
```

内置 `claude-code` 在 `permission: true` 时自动切换到 `claude --permission-prompt-tool stdio`，把 Claude 的 `control_request`/`control_response` 与 AgentProc permission 帧双向转译，从而把 Claude 工具授权接到微信 `ask` 闭环。

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
