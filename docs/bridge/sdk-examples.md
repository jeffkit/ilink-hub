# Bridge Profile 完整示例

> 最后更新：2026-06-26

本页提供三个**开箱即用**的 Bridge Profile 示例，均已通过本地运行验证：

| 示例 | 语言 | AI 工具 | 目录 |
|------|------|---------|------|
| [Claude Code（Node.js）](#claude-code-nodejs) | Node.js | Claude Code CLI | `examples/claude-code-nodejs/` |
| [Cursor Agent（Python）](#cursor-python) | Python | Cursor Agent CLI | `examples/cursor-python/` |
| [Codex（Shell）](#codex-shell) | Shell | OpenAI Codex CLI | `examples/codex-shell/` |

所有示例均：
- 通过 bridge SDK（`agentproc`）或标准 P0 协议接入
- 支持**多轮对话**（session resume）
- 可在**不启动 bridge** 的情况下单独测试

---

## Claude Code（Node.js SDK）{#claude-code-nodejs}

用 Node.js SDK 调用 Claude Code CLI，并通过 `AGENT_SESSION:` 前缀保持多轮对话上下文。

### 前提条件

```bash
node --version          # 需要 18+
npm install -g @anthropic-ai/claude-code
claude login            # 或 export ANTHROPIC_API_KEY=sk-ant-...
```

### 安装与测试

```bash
cd examples/claude-code-nodejs
npm install

# 本地模拟调用（不需要启动 bridge）
AGENT_MESSAGE="你好，介绍一下自己" \
AGENT_SESSION_ID="" \
CLAUDE_MODEL="sonnet" \
node handler.js
```

预期输出：

```
AGENT_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Claude，一个由 Anthropic 开发的 AI 助手。有什么可以帮你的吗？
```

### 接入 Bridge

修改 `profiles.yaml` 中的 `cwd` 为你的项目目录，然后：

```bash
ilink-hub-bridge --config profiles.yaml
```

### 核心代码

```javascript
// examples/claude-code-nodejs/handler.js
const { createProfile } = require('agentproc');
const { spawn } = require('child_process');

async function callClaude(message, sessionId) {
  const args = ['--print', '--output-format', 'json'];
  if (process.env.CLAUDE_MODEL) args.push('--model', process.env.CLAUDE_MODEL);
  if (sessionId) args.push('--resume', sessionId);
  args.push(message);

  const stdout = await spawnClaude(args);  // 关闭 stdin，收集 stdout

  const events = JSON.parse(stdout.trim());
  const resultEvent = [...events].reverse().find((e) => e.type === 'result');
  return {
    result: resultEvent.result || '',
    sessionId: resultEvent.is_error ? '' : (resultEvent.session_id || ''),
  };
}

createProfile(async ({ message, sessionId }) => {
  const { result, sessionId: newSessionId } = await callClaude(message, sessionId);
  return { response: result, sessionId: newSessionId || undefined };
});
```

**关键设计点：**
- `spawnClaude()` 立即关闭 stdin（`child.stdin.end()`），避免 `claude` 等待管道输入
- Claude 在 API 错误时也以非零退出码退出但会输出 JSON，所以**不以退出码判断成败**，而是解析 JSON 内容
- 当 `is_error: true` 时，不保存 session_id（避免用损坏的 session 继续对话）
- `--resume` 失败时自动降级为新会话

---

## Cursor Agent（Python SDK）{#cursor-python}

用 Python SDK 调用 Cursor Agent CLI（`agent` 命令），支持通过 `--resume` 保持对话上下文。

### 前提条件

```bash
python3 --version       # 需要 3.10+
agent --version         # Cursor Agent CLI 已安装（见 https://cursor.com/docs/cli/overview）
agent login             # 或 export CURSOR_API_KEY=key-...
```

### 安装与测试

```bash
cd examples/cursor-python
pip install -r requirements.txt

# 本地模拟调用（不需要启动 bridge）
AGENT_MESSAGE="你好，介绍一下自己" \
AGENT_SESSION_ID="" \
python3 handler.py
```

预期输出：

```
AGENT_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Cursor Agent，有什么可以帮你的吗？
```

### 接入 Bridge

修改 `profiles.yaml` 中的 `cwd` 为你的项目目录，然后：

```bash
ilink-hub-bridge --config profiles.yaml
```

### 核心代码

```python
# examples/cursor-python/handler.py
from agentproc import AgentContext, AgentResult, create_profile

async def call_cursor_agent(message: str, session_id: str) -> tuple[str, str]:
    cmd = ["agent", "--print", "--trust", "--output-format", "json"]
    if session_id:
        cmd += ["--resume", session_id]

    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    stdout_bytes, _ = await asyncio.wait_for(
        proc.communicate(input=message.encode()),
        timeout=300,
    )

    data = json.loads(stdout_bytes.decode())
    return data.get("result", ""), data.get("session_id", "")

async def handler(ctx: AgentContext) -> AgentResult:
    response, new_session_id = await call_cursor_agent(ctx.message, ctx.session_id)
    return AgentResult(response=response, session_id=new_session_id or ctx.session_id)

create_profile(handler)
```

**关键设计点：**
- 消息通过 `stdin` 传入（`proc.communicate(input=message.encode())`），兼容多行消息
- `agent --output-format json` 输出单行 JSON，包含 `result` 和 `session_id`
- `--trust` 允许 agent 在不提示确认的情况下访问当前目录（等价于 `--yolo`）

---

## Codex（Shell）{#codex-shell}

用纯 Shell 脚本调用 OpenAI Codex CLI，通过 `--json` 事件流解析回复和 session_id。

### 前提条件

```bash
codex --version         # Codex CLI 已安装
codex login             # 或 export OPENAI_API_KEY=sk-...
jq --version            # brew install jq  或  sudo apt install jq
```

### 测试

```bash
cd examples/codex-shell

# 本地模拟调用（不需要启动 bridge）
AGENT_MESSAGE="你好，介绍一下自己" \
AGENT_SESSION_ID="" \
ILINK_CWD="$(pwd)" \
bash handler.sh
```

预期输出：

```
AGENT_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？
```

### 接入 Bridge

修改 `profiles.yaml` 中的 `cwd` 为你的项目目录，然后：

```bash
ilink-hub-bridge --config profiles.yaml
```

### 核心代码

```bash
# examples/codex-shell/handler.sh
MESSAGE="${AGENT_MESSAGE:-}"
SESSION_ID="${AGENT_SESSION_ID:-}"

# 有 session_id 时用 exec resume（多轮对话），否则新建会话
if [[ -n "$SESSION_ID" ]]; then
    CODEX_ARGS=(exec resume "$SESSION_ID" "$MESSAGE")
else
    CODEX_ARGS=(exec "$MESSAGE")
fi

# 关闭 stdin（echo ""），使用 --json 获取结构化输出
JSON_OUTPUT=$(echo "" | codex "${CODEX_ARGS[@]}" \
    --dangerously-bypass-approvals-and-sandbox --json 2>/dev/null)

# 提取 session_id 和回复文本
NEW_SESSION_ID=$(printf '%s\n' "$JSON_OUTPUT" \
    | jq -r 'select(.type=="thread.started") | .thread_id // empty' | head -1)
RESPONSE=$(printf '%s\n' "$JSON_OUTPUT" \
    | jq -r 'select(.type=="item.completed" and .item.type=="agent_message") | .item.text // empty')

# P0 输出：第一行为 AGENT_SESSION:<uuid>，其余为回复正文
if [[ -n "$NEW_SESSION_ID" ]]; then echo "AGENT_SESSION:$NEW_SESSION_ID"; fi
printf '%s' "$RESPONSE"
```

**Codex JSON 事件流格式：**

```jsonl
{"type":"thread.started","thread_id":"019eac60-..."}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"你好！..."}}
{"type":"turn.completed","usage":{...}}
```

**关键设计点：**
- `echo ""` 关闭 stdin，避免 codex 等待管道输入
- `--json` 输出 JSONL 格式，便于 `jq` 精确提取
- `--dangerously-bypass-approvals-and-sandbox` 用于非交互环境（仅在受信任目录使用）
- 脚本同时支持 `jq` 和 `python3` 两种解析方式

---

## 多轮对话验证

三个示例均已通过多轮对话测试：

```bash
# 第一轮：获取 session_id
AGENT_MESSAGE="用一句话说你好" AGENT_SESSION_ID="" bash handler.sh
# 输出：AGENT_SESSION:019eac6a-...
#       你好。

# 第二轮：用上一轮的 session_id 继续对话
AGENT_MESSAGE="我上一条消息说了什么？" AGENT_SESSION_ID="019eac6a-..." bash handler.sh
# 输出：AGENT_SESSION:019eac6a-...
#       你上一条消息是："用一句话说你好"。
```

---

## 下一步

- [Profile 协议规范（P0）](/bridge/profile-spec) — 了解 stdin/stdout 约定的完整定义
- [Node.js 开发教程](/bridge/develop-nodejs) — 从零实现自定义 handler
- [Python 开发教程](/bridge/develop-python) — Python 版本教程
- [使用指引](/bridge/USAGE) — 多 CLI 配置、多项目管理
