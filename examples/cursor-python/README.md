# Cursor Agent Bridge Profile（Python SDK 版）

用 Python SDK 把 Cursor Agent CLI 接入 iLink Hub Bridge，支持多轮对话。

## 前提条件

- Python 3.10+
- [iLink Hub Bridge](https://github.com/jeffkit/ilink-hub) 已安装
- Cursor Agent CLI 已安装并登录（[安装文档](https://cursor.com/docs/cli/overview)）

```bash
agent --version       # 验证已安装
agent login           # 或 export CURSOR_API_KEY=key-...
```

## 安装

```bash
cd examples/cursor-python
pip install -r requirements.txt
# 推荐使用虚拟环境：
# python3 -m venv .venv && source .venv/bin/activate
# pip install -r requirements.txt
```

## 本地测试

不需要启动完整的 bridge，直接模拟一次调用：

```bash
ILINK_MESSAGE="你好，用一句话介绍你自己" \
ILINK_SESSION_ID="" \
ILINK_SESSION_NAME="default" \
ILINK_FROM_USER="test" \
ILINK_CONTEXT_TOKEN="test-token" \
python3 handler.py
```

预期输出（第一行为 session_id，其余为 Cursor Agent 的回复）：

```
ILINK_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Cursor Agent，有什么可以帮你的吗？
```

## 接入 Bridge

1. 修改 `profiles.yaml` 中的 `cwd` 为你的项目目录
2. 如使用虚拟环境，取消注释 `command` / `args` 字段并注释掉 `script` 字段
3. 启动 bridge：

```bash
ilink-hub-bridge --config profiles.yaml
```

4. 在微信里发消息，就能和 Cursor Agent 对话了

## 工作原理

```
微信消息 → Hub → bridge → python3 handler.py
                              │
              ┌───────────────┤
              │  SESSION_ID 非空 → agent --resume <UUID>（接续上文）
              │  SESSION_ID 为空 → agent（全新会话）
              └───────────────┤
                              │ stdout: ILINK_SESSION:<uuid>
                              │         <回复文本>
```

`handler.py` 通过 `asyncio.create_subprocess_exec` 调用 `agent --print --output-format json`，从 JSON 输出中提取回复和新 session_id，通过 `ilink-bridge-profile` SDK 按 P0 协议写回 stdout。
