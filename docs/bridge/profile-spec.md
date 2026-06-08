# Bridge Profile 规范（P0 Exec Protocol）

> 最后更新：2026-06-08

iLink Hub Bridge 的 **profile** 就是一个可执行的脚本或程序：收到消息 → 做处理 → 把回复写到 stdout。

---

## 1. P0 协议契约

P0 仅依赖环境变量 + stdout，**完全跨平台**（macOS / Linux / Windows）。

### 输入（bridge 自动注入环境变量）

| 变量名 | 说明 |
|--------|------|
| `ILINK_MESSAGE` | 用户消息文本（路由后的净文本，前缀已剥离） |
| `ILINK_SESSION_ID` | Hub 持久化的后端 session UUID（空 = 新会话） |
| `ILINK_SESSION_NAME` | session 可读名称（默认 `default`） |
| `ILINK_FROM_USER` | 发送消息的用户 ID |
| `ILINK_CONTEXT_TOKEN` | Hub context token |

> 当 YAML 设置了 `stdin: message` 时，`ILINK_MESSAGE` 同时也会写入 stdin。

### 输出（profile 写 stdout）

```
[可选] ILINK_SESSION:<uuid>     ← 如需 session 追踪，首行输出这个
<回复给微信用户的文本>
```

### 退出码

| 退出码 | 含义 |
|--------|------|
| `0` | 成功，stdout 内容作为回复 |
| 非 `0` | 失败，bridge 发送错误提示（若 `send_error_reply: true`） |

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

SDK 把读取环境变量、写 stdout、管理对话历史的样板代码封装掉，让你**只写业务逻辑**。

### Python SDK

```bash
pip install ilink-bridge-profile
```

创建 `my_handler.py`：

```python
from ilink_bridge import create_profile

async def handler(ctx):
    # ctx.message, ctx.session_id, ctx.from_user, ctx.context_token
    reply = await my_ai_call(ctx.message)
    return reply   # 直接返回字符串即可

create_profile(handler)
```

YAML 配置：

```yaml
profiles:
  my-bot:
    script: ./my_handler.py
    timeout_secs: 60
```

### Node.js SDK

```bash
npm install ilink-bridge-profile
```

创建 `my_handler.js`：

```js
const { createProfile } = require('ilink-bridge-profile');

createProfile(async ({ message, sessionId, fromUser }) => {
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
from ilink_bridge import create_profile, load_history, append_history, HistoryEntry, ProfileResult

async def handler(ctx):
    history = load_history(ctx.session_id)
    messages = [{"role": e.role, "content": e.content} for e in history]
    messages.append({"role": "user", "content": ctx.message})

    reply = await call_openai(messages)   # 传入完整上下文

    append_history(ctx.session_id, [
        HistoryEntry(role="user", content=ctx.message),
        HistoryEntry(role="assistant", content=reply),
    ])
    return ProfileResult(response=reply, session_id=ctx.session_id)

create_profile(handler)
```

---

## 4. 不用 SDK 的裸脚本

如果你只需做简单转发或不想引入依赖，直接读 env var、写 stdout 即可。

### Bash

```bash
#!/usr/bin/env bash
# my_handler.sh

REPLY=$(curl -s https://api.example.com/chat \
  -H "Authorization: Bearer $MY_API_KEY" \
  --data-urlencode "message=$ILINK_MESSAGE")

echo "$REPLY"
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
import os, sys
message = os.environ.get('ILINK_MESSAGE', '')
reply = my_ai_call(message)
sys.stdout.write(reply)
```

### Node.js（无 SDK）

```js
#!/usr/bin/env node
const message = process.env.ILINK_MESSAGE || '';
async function main() {
  const reply = await myAI(message);
  process.stdout.write(reply);
}
main().catch(e => { process.stderr.write(String(e)); process.exit(1); });
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
    stdin: message
    cwd: /path/to/your/project
    timeout_secs: 300
    cli_session_first_line_prefix: "ILINK_SESSION:"
```

手动测试：

```bash
ILINK_MESSAGE="你好" ILINK_SESSION_ID="" ilink-hub-bridge profile claude-code
```

---

## 6. 分享与发布

**团队内分享**：把脚本放进 git 仓库，其他人 clone 后，YAML 填相对路径即可：

```yaml
profiles:
  my-bot:
    script: ./scripts/my_handler.py
```

**公开发布**：发布为 npm 或 PyPI 包，包名约定 `ilink-bridge-profile-<type>`：

```bash
# 发布
npm publish            # 或 python -m twine upload dist/*

# 用户安装后，直接用 command 引用
```

```yaml
profiles:
  gemini:
    command: ilink-bridge-profile-gemini
```

---

## 7. 调试

模拟一次 bridge 调用（不启动完整 bridge）：

```bash
ILINK_MESSAGE="你好" \
ILINK_SESSION_ID="" \
ILINK_SESSION_NAME="default" \
ILINK_FROM_USER="test" \
ILINK_CONTEXT_TOKEN="test-token" \
python3 ./my_handler.py
```

或用 bridge 内置子命令调用 built-in profile：

```bash
ILINK_MESSAGE="你好" ILINK_SESSION_ID="" ilink-hub-bridge profile claude-code
```

调试消息路由：

```bash
ILINKHUB_BRIDGE_DUMP_MSG=1 ilink-hub-bridge --config my.yaml
```
