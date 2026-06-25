# Email Bridge — Agently Mail 通道

## 概述

`ilink-email-bridge`（位于 `sdk/email-bridge/`）是 Universal Agent Gateway 的邮件通道适配器。
它轮询 Agently Mail（QQ Agent 邮箱），按邮件主题前缀路由到 Profile，自动回复。

## 快速启动

```bash
# 前置：安装 agently-cli 并授权
npm install -g @tencent-qqmail/agently-cli
agently-cli auth login

# 准备配置文件
cp sdk/email-bridge/email-profiles.example.yaml my-profiles.yaml
# 编辑 my-profiles.yaml，配置你的 Profile

# 启动（每 5 分钟轮询）
node sdk/email-bridge/bin/cli.js --config my-profiles.yaml

# 调试（30 秒轮询，不实际回复）
node sdk/email-bridge/bin/cli.js --config my-profiles.yaml --interval 30000 --dry-run
```

## 邮件路由规则

主题前缀 `[profile-name]` 决定由哪个 Profile 处理：

```
Subject: [cursor] 帮我 review 这段代码   → cursor Profile
Subject: [claude] 写一份设计文档          → claude-code Profile
Subject: 你好，有问题想请教              → 默认 Profile（email-profiles.yaml 中 default 字段）
```

## 内置 Profile

无需额外安装，开箱即用（依赖对应 CLI 存在于 PATH）：

| 主题前缀 | 调用的 CLI | 说明 |
|---------|-----------|------|
| `[claude]` | `claude` | Claude Code |
| `[cursor]` | `agent` | Cursor Agent |
| `[codebuddy]` | `codebuddy` | CodeBuddy Code |
| `[codex]` | `codex` | OpenAI Codex |
| `[agy]` | `agy` | Antigravity (Google DeepMind) |
| `[echo]` | 内置 | 调试回显 |

## 关键特性

### 防循环：自发邮件过滤
跳过 `from.email == 我方 Agent 邮箱` 的邮件，防止 Agent 对自己回复再次处理。

### 线程会话保持
同一邮件线程（由 RFC 2822 `References` 头部确定）共享同一个 AI Session，AI 能记住整个对话上下文。

### 邮件正文清理
传给 AI Profile 前自动：
1. 剥离 HTML 标签
2. 移除 quoted 引用行（`>` 开头、"On X wrote:" 等）
3. 移除 Agently Mail 自动签名
4. 截断超长正文（默认 8000 字）

## 配置文件（email-profiles.yaml）

```yaml
default: claude-code

profiles:
  claude-code:
    command: node
    args:
      - ./profiles/claude-code.js  # 使用内置 claude-code Profile
    description: Claude Code AI 助手（默认）
    trigger: claude

  cursor:
    command: node
    args:
      - ./profiles/cursor.js
    description: Cursor AI 编程助手
    trigger: cursor
```

`args` 路径相对于 yaml 文件所在目录。

## 程序化使用

```js
const { createEmailBridge } = require('ilink-email-bridge');

createEmailBridge({
  profilesConfig: './email-profiles.yaml',
  pollIntervalMs: 5 * 60_000,
  filterSelfSent: true,   // 默认 true
  dryRun: false,
});
```

## 与 iLink Hub 的关系

见 [架构方案](../../sdk/email-bridge/docs/ARCHITECTURE.md)。

两者是**同级通道**，共用 Profile 执行层（P0 协议），不存在依赖或替代关系。
