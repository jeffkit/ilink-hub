# Codex Bridge Profile（Shell 版）

用纯 Shell 脚本把 OpenAI Codex CLI 接入 iLink Hub Bridge，支持多轮对话。

## 前提条件

- [iLink Hub Bridge](https://github.com/jeffkit/ilink-hub) 已安装
- Codex CLI 已安装并登录

```bash
codex --version       # 验证已安装
codex login           # 或 export OPENAI_API_KEY=sk-...
```

- `jq` 已安装（用于解析 JSONL 输出）

```bash
brew install jq       # macOS
sudo apt install jq   # Ubuntu/Debian
```

## 本地测试

不需要启动完整的 bridge，直接模拟一次调用：

```bash
AGENT_MESSAGE="你好，用一句话介绍你自己" \
AGENT_SESSION_ID="" \
AGENT_CWD="$(pwd)" \
bash handler.sh
```

预期输出（第一行为 session_id，其余为 Codex 的回复）：

```
AGENT_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？
```

## 接入 Bridge

1. 修改 `profiles.yaml` 中的 `cwd` 为你的项目目录
2. 启动 bridge：

```bash
ilink-hub-bridge --config profiles.yaml
```

3. 在微信里发消息，就能和 Codex 对话了

## 工作原理

```
微信消息 → Hub → bridge → bash handler.sh
                              │
              ┌───────────────┤
              │  SESSION_ID 非空 → codex exec resume <UUID> <消息>（接续上文）
              │  SESSION_ID 为空 → codex exec <消息>（全新会话）
              └───────────────┤
                              │ stdout: AGENT_SESSION:<uuid>
                              │         <回复文本>
```

`handler.sh` 调用 `codex exec --json`，从 JSONL 事件流中提取：
- `thread.started` 事件的 `thread_id` → 作为新 session_id 写到第一行
- `item.completed` 事件的 `item.text` → 作为回复正文

支持 `jq`（首选）和 `python3`（备用）两种 JSON 解析方式。
