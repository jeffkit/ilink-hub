# Bridge Profile 规范（AgentProc 0.3 NDJSON Protocol）

> 最后更新：2026-07-13

iLink Hub Bridge 的 **profile** 就是一个可执行的脚本或程序：从 stdin 读一行 NDJSON turn → 做处理 → 在 stdout 逐行输出 NDJSON 事件。这是对旧版 P0（环境变量 + sentinel 前缀）的**硬切换**，不再保留双协议兼容。

协议常量：`PROTOCOL_VERSION = "0.3"`。

---

## 1. AgentProc 0.3 协议契约

0.3 仅依赖 **stdin + stdout 的 NDJSON**，**完全跨平台**（macOS / Linux / Windows）。环境变量只用于密钥与配置，不再承载消息/session 等业务字段。

### 输入（bridge 向 profile stdin 写一行 NDJSON turn）

```json
{
  "type": "turn",
  "message": "用户消息文本（路由后的净文本，前缀已剥离）",
  "session_id": "Hub 持久化的后端 session UUID（空 = 新会话）",
  "from_user": "发送消息的用户 ID",
  "protocol_version": "0.3",
  "session_name": "default",
  "attachments": [
    {"kind": "image", "url": "https://...", "filename": "a.png", "mime_type": "image/png", "size": 12345}
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
| `attachments` | 附件数组；`kind` ∈ {`image`, `file`, `video`}，`url` 必填，其余元数据可选 |
| `permission` | `true` = 开启 permission 通道（stdin 保持开启以接收 `permission_response`） |

写入规则：

- `permission: false`（默认）：bridge 写完 turn 后**立即关闭 stdin**；
- `permission: true`：stdin 保持开启，供后续写入 `permission_response` 帧（见 §Permission）。

> 例外：`AGENT_CONTEXT_TOKEN` 仍由 Hub 作为环境变量注入，用于回调凭证（ilink-hub 扩展，非 AgentProc 标准）。

### 输出（profile 向 stdout 逐行写 NDJSON 事件）

```json
{"type":"partial","text":"流式片段","role":"output"}
{"type":"text","text":"最终回复片段"}
{"type":"session","id":"<uuid>"}
{"type":"error","message":"可读错误文本"}
{"type":"permission_request","id":"req-42","tool":"Bash","input":{...}}
```

| 事件 | 用途 | bridge 行为 |
|------|------|-------------|
| `partial` | 流式片段（`role`: `output`（默认）/ `thinking`） | `streaming` hint 为真时**立即**转发给用户；否则忽略 |
| `text` | 最终回复片段（可多次，bridge 拼接） | 累积为最终 body；streaming 模式下若已有 partial 转发则去重丢弃 |
| `session` | 上报 session id（last-wins） | 持久化为 Hub session ID |
| `error` | 终端错误 | 标记 turn 失败；错误文本交付用户 |
| `permission_request` | 工具授权请求（仅 `permission: true`） | 按 `permission_default` 回 `permission_response` |

事件采用**封闭词汇表**：未知 `type` 静默忽略。Bridge 从进程启动起**实时读取** stdout——profile 一旦写入并刷新缓冲区，bridge 立即处理。

#### `partial` vs `text`

- `partial` = 流式增量，仅供 streaming 模式实时推送，**不**进入最终 body。
- `text` = 最终回复的权威内容；非 streaming 模式下作为唯一回复发出，streaming 模式下若 partial 已转发则被去重。

#### 关闭流式（`streaming: false`）

`streaming` 是 **bridge 侧 hint**，决定是否转发 `partial`。profile 始终可以发 `partial` + `text`，bridge 依据 hint 取舍——agent 侧无需感知模式分支。

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/your/project
    streaming: false     # bridge 不转发 partial，仅以 text 事件作为最终回复
```

默认值为 `true`（流式开启）。

**Bash 示例（NDJSON 事件）：**

```bash
#!/usr/bin/env bash
# 流式调用示例：每秒发一段 partial，最后发 text + session
printf '{"type":"partial","text":"第一段，1 秒前发出"}\n'
sleep 1
printf '{"type":"partial","text":"第二段，2 秒前发出"}\n'
sleep 1
printf '{"type":"text","text":"第一段，1 秒前发出\n第二段，2 秒前发出"}\n'
# 进程退出 → bridge 读到 EOF → 完成
```

### Permission 通道（可选）

profile 设置 `permission: true` 时开启：

1. profile 在需要工具授权时输出 `{"type":"permission_request","id":...,"tool":...,"input":...}`
2. bridge 依据 `permission_default` 策略（`allow` / `deny` / `ask`）决定，并通过 stdin 写回一行：

```json
{"type":"permission_response","id":"req-42","behavior":"allow","message":"可选说明"}
```

3. profile 收到响应后继续执行

> 当前阶段实现 framing 转换与 default policy（`allow`/`deny`）；`ask` 的完整 WeChat 交互审批循环为后续产品特性。

### 退出码优先级

turn 的成败按以下优先级判定（前者覆盖后者）：

1. **timeout** — 进程超时 → turn 失败
2. **error 事件** — 收到 `{"type":"error",...}` → turn 失败（错误文本已交付用户）
3. **进程 exit code** — 非零退出但已恢复 session/body 时容错；`0` = 成功

Stderr 记录为 debug 日志，不发给用户（除非 `include_stderr_in_reply: true`）。

---

## 2. bridge 怎么运行你的脚本：`script:` 字段

在 YAML 里写 `script:` 即可——bridge 根据**文件扩展名**自动推断运行时，无需你手动写 `command` / `args`：

```yaml
profiles:
  my-bot:
    script: ./my_handler.py   # bridge 自动调用 python3 my_handler.py
    timeout_secs: 60
```

| 扩展名 | 推断运行时 |
|--------|-----------|
| `.py` | `python3 <script>` |
| `.js` / `.mjs` | `node <script>` |
| `.ts` | `npx tsx <script>` |
| `.sh` / `.bash` | `bash <script>` |
| `.rb` | `ruby <script>` |
| 无 / 其他 | 直接执行（需 chmod +x + shebang） |

如果你需要用特定的 Python 虚拟环境或 Python 路径，设置 `command` 即可覆盖自动推断：

```yaml
profiles:
  my-bot:
    script: ./my_handler.py    # 仅作标注，command 优先
    command: .venv/bin/python3
    args: ["./my_handler.py"]
```

---

## 3. 用 SDK 写 profile（推荐）

SDK 把读取 stdin turn、写 NDJSON 事件、管理对话历史的样板代码封装掉，让你**只写业务逻辑**。SDK 内部就是按 0.3 NDJSON 与 bridge 通信。

### Python SDK

```bash
pip install agentproc
```

创建 `my_handler.py`：

```python
from agentproc import create_profile

async def handler(ctx):
    # ctx.message, ctx.session_id, ctx.from_user, ctx.attachments
    reply = await my_ai_call(ctx.message)
    return reply   # 直接返回字符串即可，SDK 发 {"type":"text",...}

create_profile(handler)
```

**流式输出（`ctx.send_partial`）：**

```python
from agentproc import create_profile, AgentResult

async def handler(ctx):
    new_sid = ctx.session_id
    # AI 每产生一段文本，立即发给用户，无需等到全部完成
    async for chunk, new_sid in stream_ai(ctx.message, ctx.session_id):
        await ctx.send_partial(chunk)   # 发 {"type":"partial","text":...} + flush
    # 全部已流式发出；如需保证非 streaming 消费者也能收到，再发一次 text
    return AgentResult(response="", session_id=new_sid)

create_profile(handler)
```

`send_partial` 的实现就是写一行 `{"type":"partial","text":<json>}\n` 并立即 `flush()`。Bridge 在实时读 stdout 时解析该事件，就立即向 Hub 发消息——**profile 完全不感知 iLink 协议**。

YAML 配置：

```yaml
profiles:
  my-bot:
    script: ./my_handler.py
    timeout_secs: 60
```

### Node.js SDK

```bash
npm install agentproc
```

创建 `my_handler.js`：

```js
const { createProfile } = require('agentproc');

createProfile(async ({ message, sessionId, fromUser, attachments }) => {
  const reply = await myAICall(message);
  return { response: reply };
});
```

YAML 配置：

```yaml
profiles:
  my-bot:
    script: ./my_handler.js
    timeout_secs: 60
```

### 多轮对话历史（JSONL，可选）

当你直接调用 LLM API 且需要携带上下文时，SDK 提供历史管理（存储于 `~/.ilink-hub/sessions/<session_id>.jsonl`）：

```python
from agentproc import create_profile, load_history, append_history, HistoryEntry, AgentResult

async def handler(ctx):
    history = load_history(ctx.session_id)
    messages = [{"role": e.role, "content": e.content} for e in history]
    messages.append({"role": "user", "content": ctx.message})

    reply = await call_openai(messages)   # 传入完整上下文

    append_history(ctx.session_id, [
        HistoryEntry(role="user", content=ctx.message),
        HistoryEntry(role="assistant", content=reply),
    ])
    return AgentResult(response=reply, session_id=ctx.session_id)

create_profile(handler)
```

---

## 4. 不用 SDK 的裸脚本

如果你只需做简单转发或不想引入依赖，直接从 stdin 读 turn、向 stdout 写 NDJSON 事件即可。

### Bash

```bash
#!/usr/bin/env bash
# my_handler.sh —— 读 stdin NDJSON turn，调外部 API，写 text 事件

TURN=$(cat)                       # 读一行 NDJSON turn
MESSAGE=$(printf '%s' "$TURN" | jq -r '.message')

REPLY=$(curl -s https://api.example.com/chat \
  -H "Authorization: Bearer $MY_API_KEY" \
  --data-urlencode "message=$MESSAGE")

# 用 jq 安全转义后输出 text 事件
jq -nc --arg t "$REPLY" '{"type":"text","text":$t}'
```

YAML：

```yaml
profiles:
  my-bot:
    script: ./my_handler.sh
```

### Python（无 SDK）

```python
#!/usr/bin/env python3
import json, sys

turn = json.loads(sys.stdin.readline())
reply = my_ai_call(turn["message"])
print(json.dumps({"type": "text", "text": reply}), flush=True)
```

### Node.js（无 SDK）

```js
#!/usr/bin/env node
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const turn = JSON.parse(line);
  myAI(turn.message).then((reply) => {
    process.stdout.write(JSON.stringify({ type: 'text', text: reply }) + '\n');
  }).catch((e) => {
    process.stdout.write(JSON.stringify({ type: 'error', message: String(e) }) + '\n');
    process.exit(1);
  });
});
```

---

## 5. 内置 profile：`type: claude-code`

由 `ilink-hub-bridge` 自带，无需额外脚本，最简单地接入 Claude Code：

```yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/your/project
    timeout_secs: 300
```

bridge 解析时自动展开为（等价于）：

```yaml
profiles:
  claude:
    command: ilink-hub-bridge
    args: [profile, claude-code]
    cwd: /path/to/your/project
    timeout_secs: 300
```

手动测试（向 stdin 写一行 turn）：

```bash
echo '{"type":"turn","message":"你好","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | ilink-hub-bridge profile claude-code
```

---

## 6. 分享与发布

**团队内分享**：把脚本放进 git 仓库，其他人 clone 后，YAML 填相对路径即可：

```yaml
profiles:
  my-bot:
    script: ./scripts/my_handler.py
```

**公开发布**：发布为 npm 或 PyPI 包，包名约定 `agentproc-<type>`：

```bash
# 发布
npm publish            # 或 python -m twine upload dist/*

# 用户安装后，直接用 command 引用
```

```yaml
profiles:
  gemini:
    command: agentproc-gemini
```

---

## 7. 调试

模拟一次 bridge 调用（不启动完整 bridge），向 stdin 写一行 turn NDJSON：

```bash
echo '{"type":"turn","message":"你好","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | python3 ./my_handler.py
```

或用 bridge 内置子命令调用 built-in profile：

```bash
echo '{"type":"turn","message":"你好","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | ilink-hub-bridge profile claude-code
```

调试消息路由：

```bash
ILINKHUB_BRIDGE_DUMP_MSG=1 ilink-hub-bridge --config my.yaml
```

---

## 8. 从 0.2 迁移要点

- 输入：`AGENT_MESSAGE` / `AGENT_SESSION_ID` / `AGENT_STREAMING` 等 env → stdin NDJSON turn
- 输出：`AGENT_PARTIAL:` / `AGENT_SESSION:` / `AGENT_ERROR:` sentinel 行 → `{"type":"partial|session|error",...}` NDJSON 事件
- 附件：`AGENT_IMAGE_URL` / `AGENT_FILE_URL` env → turn 对象的 `attachments` 数组
- 最终回复：stdout 末尾的自由文本 → `{"type":"text",...}` 事件
- 流式：`AGENT_STREAMING` env 分支取消，profile 统一发 `partial` + `text`，由 bridge hint 决定转发
- YAML：移除 `stdin`（`StdinMode`）与 `cli_session_first_line_prefix` 字段；新增 `env_allowlist` / `kill_grace_secs` / `permission` / `permission_default` / `truncation_suffix` / `{{PROFILE_DIR}}`
