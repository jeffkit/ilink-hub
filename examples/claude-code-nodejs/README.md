# Claude Code Bridge Profile（Node.js SDK 版）

用 Node.js SDK 把 Claude Code CLI 接入 iLink Hub Bridge，支持多轮对话。

## 前提条件

- Node.js 18+
- [iLink Hub Bridge](https://github.com/jeffkit/ilink-hub) 已安装
- Claude Code CLI 已安装并登录

```bash
npm install -g @anthropic-ai/claude-code
claude login          # 或 export ANTHROPIC_API_KEY=sk-ant-...
claude --version      # 验证
```

## 安装

```bash
cd examples/claude-code-nodejs
npm install
```

## 本地测试

不需要启动完整的 bridge，直接模拟一次调用：

```bash
ILINK_MESSAGE="你好，用一句话介绍你自己" \
ILINK_SESSION_ID="" \
ILINK_SESSION_NAME="default" \
ILINK_FROM_USER="test" \
ILINK_CONTEXT_TOKEN="test-token" \
node handler.js
```

或使用 npm test：

```bash
npm test
```

预期输出（第一行为 session_id，其余为 Claude 的回复）：

```
ILINK_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Claude，一个由 Anthropic 开发的 AI 助手。有什么可以帮你的吗？
```

## 接入 Bridge

1. 修改 `profiles.yaml` 中的 `cwd` 为你的项目目录
2. 启动 bridge：

```bash
ilink-hub-bridge --config profiles.yaml
```

3. 在微信里发消息，就能和 Claude 对话了

**支持的指令：**

| 消息 | 效果 |
|------|------|
| 任意文字 | 接续上次对话（自动 --resume）|
| `/new <问题>` | 强制开启新会话 |

## 工作原理

```
微信消息 → Hub → bridge → node handler.js
                              │
              ┌───────────────┤
              │  SESSION_ID 非空 → claude --resume <UUID>（接续上文）
              │  SESSION_ID 为空 → claude（全新会话）
              └───────────────┤
                              │ stdout: ILINK_SESSION:<uuid>
                              │         <回复文本>
```

`handler.js` 调用 `claude --print --output-format json`，从 JSON 输出中提取回复和新 session_id，通过 `ilink-bridge-profile` SDK 按 P0 协议写回 stdout。
