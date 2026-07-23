# Codex Bridge Profile（Shell 版）

> **Bridge 已迁出**：bridge（原 `ilink-hub-bridge`）已拆到独立项目
> [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)，二进制改名为 `im-agentproc`。
> 本示例的 profile 协议与 handler 思路不变，命令以 im-agentproc 为准。

用纯 Shell 脚本把 OpenAI Codex CLI 接入 iLink Hub Bridge，支持多轮对话。采用 AgentProc 0.4 NDJSON 协议与 bridge 通信。

## 前提条件

- [iLink Hub](https://github.com/jeffkit/ilink-hub)（Hub 服务）与 [im-agentproc](https://github.com/jeffkit/im-agentproc)（bridge）已安装
- Codex CLI 已安装并登录

```bash
codex --version       # 验证已安装
codex login           # 或 export OPENAI_API_KEY=sk-...
```

- `jq` 已安装（用于解析 JSONL 输出与编码 NDJSON 事件）

```bash
brew install jq       # macOS
sudo apt install jq   # Ubuntu/Debian
```

## 本地测试

不需要启动完整的 bridge，向 stdin 写一行 turn NDJSON 即可模拟一次调用：

```bash
echo '{"type":"turn","message":"你好，用一句话介绍你自己","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | AGENT_CWD="$(pwd)" bash handler.sh
```

预期输出（NDJSON 事件流，最后一行为 session 事件）：

```
{"type":"partial","text":"你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？"}
{"type":"text","text":"你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？"}
{"type":"session","id":"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"}
```

## 接入 Bridge

1. 修改 `profiles.yaml` 中的 `cwd` 为你的项目目录
2. 启动 bridge：

```bash
im-agentproc --config profiles.yaml
```

3. 在微信里发消息，就能和 Codex 对话了

## 工作原理

```
微信消息 → Hub → bridge → bash handler.sh
                              │ stdin: 一行 NDJSON turn
                              │   {type:"turn", message, session_id, ...}
                              │
              ┌───────────────┤
              │  session_id 非空 → codex exec resume <UUID> <消息>（接续上文）
              │  session_id 为空 → codex exec <消息>（全新会话）
              └───────────────┤
                              │ stdout: NDJSON 事件流
                              │   {"type":"partial","text":...}   ← 实时分块
                              │   {"type":"text","text":...}      ← 最终回复
                              │   {"type":"session","id":...}     ← session id
```

`handler.sh` 调用 `codex exec --json`，从 JSONL 事件流中提取：
- `thread.started` 事件的 `thread_id` → 发 `session` 事件
- `item.completed (agent_message)` 事件的 `item.text` → 发 `partial` 事件并累积进最终 `text` 事件
