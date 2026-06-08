# 用 Python 开发 Bridge Profile

> 最后更新：2026-06-08

本教程带你从零开始，用 Python 编写一个能接收微信消息、调用 AI API、返回回复的 Bridge Profile，并将它接入 iLink Hub Bridge。

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
pip install ilink-bridge-profile
```

---

## 第二步：编写 handler

创建 `handler.py`：

```python
from ilink_bridge import create_profile, ProfileContext

async def handler(ctx: ProfileContext) -> str:
    # 这里写你的 AI 调用逻辑
    # 示例：简单的 echo 回复
    return f"你说的是：{ctx.message}"

create_profile(handler)
```

`create_profile` 帮你做了所有样板工作：
- 读取 `ILINK_MESSAGE`、`ILINK_SESSION_ID` 等环境变量
- 调用你的 async handler 函数
- 按 P0 协议把回复写到 stdout

### 调用真实 AI（以 OpenAI 为例）

```bash
pip install openai
```

```python
import os
from openai import AsyncOpenAI
from ilink_bridge import create_profile, ProfileContext

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: ProfileContext) -> str:
    completion = await client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": ctx.message}],
    )
    return completion.choices[0].message.content

create_profile(handler)
```

---

## 第三步：本地测试

不需要启动完整的 bridge，直接用环境变量模拟调用：

```bash
ILINK_MESSAGE="你好，介绍一下自己" \
ILINK_SESSION_ID="" \
ILINK_FROM_USER="test-user" \
python3 handler.py
```

你会看到类似这样的输出：

```
你说的是：你好，介绍一下自己
```

---

## 第四步：接入 Bridge

创建 `profiles.yaml`：

```yaml
profiles:
  my-ai:
    script: ./handler.py    # bridge 自动用 python3 运行
    timeout_secs: 60

routing:
  default_profile: my-ai
```

> **使用虚拟环境时**：需要用 `command` 指定解释器路径：
>
> ```yaml
> profiles:
>   my-ai:
>     script: ./handler.py
>     command: ./.venv/bin/python3
>     args: ["./handler.py"]
>     timeout_secs: 60
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
from ilink_bridge import (
    create_profile, ProfileContext, ProfileResult,
    load_history, append_history, HistoryEntry,
)

client = AsyncOpenAI(api_key=os.environ["OPENAI_API_KEY"])

async def handler(ctx: ProfileContext) -> ProfileResult:
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

    return ProfileResult(response=reply, session_id=ctx.session_id)

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
from ilink_bridge import create_profile, ProfileContext

client = anthropic.AsyncAnthropic(api_key=os.environ["ANTHROPIC_API_KEY"])

async def handler(ctx: ProfileContext) -> str:
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
profiles:
  claude-api:
    script: ./claude-profile.py
    env:
      ANTHROPIC_API_KEY: "your-api-key-here"
    timeout_secs: 60

routing:
  default_profile: claude-api
```

---

## 发布为 PyPI 包（可选）

如果你想分享 profile 给其他人，先在 `pyproject.toml` 中配置入口点：

```toml
[build-system]
requires = ["setuptools>=42"]
build-backend = "setuptools.build_meta"

[project]
name = "ilink-bridge-profile-myai"
version = "0.1.0"
dependencies = ["ilink-bridge-profile", "openai"]

[project.scripts]
ilink-bridge-profile-myai = "myai_profile:main"
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
profiles:
  my-ai:
    command: ilink-bridge-profile-myai
```

---

## 下一步

- [P0 协议规范](/bridge/profile-spec) — 完整技术规范
- [Node.js 版本教程](/bridge/develop-nodejs) — 用 Node.js 编写同等功能的 Profile
- [接入 Claude Code](/guide/claude-code) — 使用内置 claude-code profile
