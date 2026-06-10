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
| **P0 协议** | bridge 与 handler 间的通信协议：env var 输入 + stdout 输出 |
| **type: claude-code** | 内置类型，自动处理 Claude Code CLI 的 `--resume` 会话续接 |
| **script:** | 指定脚本路径，bridge 按扩展名自动推断运行时（.py/.js/.ts/.sh/.rb） |
| **SDK** | `ilink-bridge-profile`（Python/Node.js），封装 P0 协议样板代码 |

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
    env:
      ILINK_SESSION_ID: ""  # 空 SESSION_ID → 内置 handler 跳过 --resume，开新会话

routing:
  strategy: prefix
  default_profile: claude
  prefix_rules:
    - prefix: "/new "       # "/new 帮我写函数" → MESSAGE = "帮我写函数"
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
    stdin: none
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
| `type` | 内置类型：`claude-code` |
| `script` | 脚本路径，自动推断运行时 |
| `command` | 显式命令（优先级高于 script/type） |
| `args` | 支持占位符 `{{MESSAGE}}` / `{{SESSION_ID}}` / `{{SESSION_NAME}}` |
| `stdin` | `none`（默认）或 `message`（把消息写入 stdin） |
| `cwd` | 工作目录，支持 `~` 展开 |
| `env` | 注入的环境变量 |
| `timeout_secs` | 超时（默认 60） |
| `max_reply_chars` | 回复最大字符数（默认 4000） |
| `include_stderr_in_reply` | 是否附加 stderr（默认 false） |

---

## Step 3：用 SDK 开发自定义 Handler {#sdk-development}

当内置类型不满足需求，需要调用 LLM API、自定义逻辑、多轮对话时，使用 SDK 编写 handler。

### P0 协议说明

bridge 通过以下 env var 传入输入，handler 向 stdout 写出输出：

| 输入 env var | 说明 |
|-------------|------|
| `ILINK_MESSAGE` | 用户消息文本（前缀路由后的净文本） |
| `ILINK_SESSION_ID` | Hub 持久化的 session UUID（空=新会话） |
| `ILINK_SESSION_NAME` | session 可读名称（默认 `default`） |
| `ILINK_FROM_USER` | 发送消息的用户 ID |
| `ILINK_CONTEXT_TOKEN` | Hub context token |

**stdout 输出格式：**
```
[可选首行] ILINK_SESSION:<uuid>   ← 若需 session 追踪
<回复给微信用户的文本>
```

---

### Python SDK

```bash
mkdir my-profile && cd my-profile
python3 -m venv .venv && source .venv/bin/activate
pip install ilink-bridge-profile
```

**最简 handler（`handler.py`）：**

```python
from ilink_bridge import create_profile, ProfileContext

async def handler(ctx: ProfileContext) -> str:
    return f"你说的是：{ctx.message}"

create_profile(handler)
```

**接 OpenAI/Claude API（`handler.py`）：**

```python
import os
import anthropic
from ilink_bridge import create_profile, ProfileContext

client = anthropic.AsyncAnthropic(api_key=os.environ["ANTHROPIC_API_KEY"])

async def handler(ctx: ProfileContext) -> str:
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
from ilink_bridge import (
    create_profile, ProfileContext, ProfileResult,
    load_history, append_history, HistoryEntry,
)

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: ProfileContext) -> ProfileResult:
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
    return ProfileResult(response=reply, session_id=ctx.session_id)

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
npm install ilink-bridge-profile
```

**最简 handler（`handler.js`）：**

```js
const { createProfile } = require('ilink-bridge-profile');

createProfile(async ({ message }) => {
  return `你说的是：${message}`;
});
```

**接 OpenAI/Claude API（`handler.js`）：**

```js
const { createProfile } = require('ilink-bridge-profile');
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
const { createProfile, loadHistory, appendHistory } = require('ilink-bridge-profile');
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

不启动完整 bridge，直接模拟 P0 协议调用：

```bash
# 测试内置 claude-code
ILINK_MESSAGE="你好" ILINK_SESSION_ID="" \
  ilink-hub-bridge profile claude-code

# 测试自定义 Python handler
ILINK_MESSAGE="你好" ILINK_SESSION_ID="" ILINK_SESSION_NAME="default" \
  ILINK_FROM_USER="test" ILINK_CONTEXT_TOKEN="test-token" \
  python3 /path/to/handler.py

# 测试自定义 JS handler
ILINK_MESSAGE="你好" ILINK_SESSION_ID="" ILINK_SESSION_NAME="default" \
  ILINK_FROM_USER="test" ILINK_CONTEXT_TOKEN="test-token" \
  node /path/to/handler.js
```

**验证输出：**
- 若管理 session，首行应为 `ILINK_SESSION:<uuid>`
- 其余行是给用户的回复文本
- 退出码 0 = 成功

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

`env:` 字段里的 `${VAR_NAME}` 会被原样作为字符串传给子进程（**当前没有自动展开**），
所以 YAML 入 git 是安全的，但**运行时必须有同名 env var**——否则子进程收到的是字面字符串 `${VAR_NAME}`，请求会报 401。

### 推荐做法（当前唯一支持的）

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

### 计划中的特性

`${VAR}` shell 插值（启动 manager 时从进程 env 自动展开）正在设计中，见
`docs/bridge/env-interpolation-spec.md`（需求已写，待实现）。在特性上线前，
**测试 YAML 时务必先确认子进程能拿到 key**（`env | grep KEY_NAME` 验证）。

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
