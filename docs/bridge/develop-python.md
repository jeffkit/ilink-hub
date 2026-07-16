# 用 Python 开发 Bridge Profile

> **2026-07-16**：Profile YAML 已改为 `agentproc:` hub form（一文件一 profile）。完整说明见 [profile-protocol](../knowledge/bridges/profile-protocol.md)。

> 最后更新：2026-07-13

本教程带你从零开始，用 Python 编写一个能接收微信消息、调用 AI API、返回回复的 Bridge Profile，并将它接入 iLink Hub Bridge。Profile 通过 **AgentProc 0.4 NDJSON 协议**与 bridge 通信：从 stdin 读一行 turn 对象，在 stdout 逐行输出 NDJSON 事件。

---

## 前置要求

- Python 3.10+
- 已安装并运行 `ilink-hub`（参见[快速开始](/guide/getting-started)）
- 已安装 `ilink-hub-bridge`（`brew install jeffkit/tap/ilink-hub`）

---

## 第一步：创建项目

```bash
mkdir my-ai-profile && cd my-ai-profile
python3 -m venv .venv
source .venv/bin/activate   # Windows: .venv\Scripts\activate
pip install "agentproc>=0.9"
```

---

## 第二步：编写 handler

创建 `handler.py`：

```python
from agentproc import create_profile, AgentContext

async def handler(ctx: AgentContext) -> str:
    # 这里写你的 AI 调用逻辑
    # 示例：简单的 echo 回复
    return f"你说的是：{ctx.message}"

create_profile(handler)
```

`create_profile` 帮你做了所有样板工作：
- 从 stdin 读取 NDJSON turn 对象（`message` / `session_id` / `from_user` / `attachments` 等）
- 调用你的 async handler 函数
- 按 AgentProc 0.4 协议把回复作为 `{"type":"text",...}` 事件写到 stdout

### 调用真实 AI（以 OpenAI 为例）

```bash
pip install openai
```

```python
import os
from openai import AsyncOpenAI
from agentproc import create_profile, AgentContext

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: AgentContext) -> str:
    completion = await client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": ctx.message}],
    )
    return completion.choices[0].message.content

create_profile(handler)
```

---

## 第三步：本地测试

不需要启动完整的 bridge，向 stdin 写一行 turn NDJSON 即可模拟调用：

```bash
echo '{"type":"turn","message":"你好，介绍一下自己","session_id":"","from_user":"test-user","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | python3 handler.py
```

你会看到类似这样的 NDJSON 事件输出：

```
{"type":"text","text":"你说的是：你好，介绍一下自己"}
```

---

## 第四步：接入 Bridge

创建 `profiles.yaml`：

```yaml
description: my AI
script: ./handler.py    # bridge 自动用 python3 运行
agentproc:
  timeout_secs: 60
```

> **使用虚拟环境时**：在 `agentproc` 里用 `command` 指定解释器：
>
> ```yaml
> description: my AI
> agentproc:
>   command: ./.venv/bin/python3
>   args: ["./handler.py"]
>   timeout_secs: 60
> ```

启动 bridge：

```bash
ilink-hub-bridge --config profiles.yaml
```

现在发微信消息给你的 ClawBot，就会触发 `handler.py`！

---

## 第五步：支持多轮对话

如果你直接调用 LLM API（不用 Claude Code），可以用 SDK 的历史管理功能保存上下文：

```python
import os
from openai import AsyncOpenAI
from agentproc import (
    create_profile, AgentContext, AgentResult,
    load_history, append_history, HistoryEntry,
)

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: AgentContext) -> AgentResult:
    # 读取历史对话
    history = load_history(ctx.session_id)
    messages = [
        {"role": "system", "content": "你是一个友好的 AI 助手。"},
        *[{"role": e.role, "content": e.content} for e in history],
        {"role": "user", "content": ctx.message},
    ]

    completion = await client.chat.completions.create(
        model="gpt-4o-mini",
        messages=messages,
    )
    reply = completion.choices[0].message.content

    # 写入历史，下次对话时使用
    append_history(ctx.session_id, [
        HistoryEntry(role="user", content=ctx.message),
        HistoryEntry(role="assistant", content=reply),
    ])

    return AgentResult(response=reply, session_id=ctx.session_id)

create_profile(handler)
```

历史文件保存在 `~/.ilink-hub/sessions/<session_id>.jsonl`，自动按会话隔离。

---

## 完整示例：接入 Anthropic Claude API

```bash
pip install anthropic
```

```python
# claude-profile.py
import os
import anthropic
from agentproc import create_profile, AgentContext

client = anthropic.AsyncAnthropic(api_key=os.environ["ANTHROPIC_API_KEY"])

async def handler(ctx: AgentContext) -> str:
    message = await client.messages.create(
        model="claude-opus-4-5",
        max_tokens=1024,
        messages=[{"role": "user", "content": ctx.message}],
    )
    return message.content[0].text

create_profile(handler)
```

`profiles.yaml`：

```yaml
description: claude API handler
script: ./claude-profile.py
agentproc:
  timeout_secs: 60
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}   # 勿写明文 key
```

---

## 发布为 PyPI 包（可选）

如果你想分享 profile 给其他人，先在 `pyproject.toml` 中配置入口点：

```toml
[build-system]
requires = ["setuptools>=42"]
build-backend = "setuptools.build_meta"

[project]
name = "agentproc-myai"
version = "0.1.0"
dependencies = ["agentproc", "openai"]

[project.scripts]
agentproc-myai = "myai_profile:main"
```

在 `myai_profile.py` 中把 `create_profile(handler)` 放到 `main()` 函数：

```python
def main():
    create_profile(handler)
```

发布：

```bash
pip install build twine
python -m build
python -m twine upload dist/*
```

其他用户安装后，在 YAML 中用 `command` 引用：

```yaml
description: my AI package
agentproc:
  command: agentproc-myai
```

---

## 下一步

- [AgentProc 0.4 协议规范](/bridge/profile-spec) — 完整技术规范
- [Node.js 版本教程](/bridge/develop-nodejs) — 用 Node.js 编写同等功能的 Profile
- [接入 Claude Code](/guide/claude-code) — 使用内置 claude-code profile
