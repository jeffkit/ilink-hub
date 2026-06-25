# AGENTS.md — ilink-email-bridge

ilink-email-bridge 是 **Universal Agent Gateway** 的邮件通道适配器。
它轮询 Agently Mail（QQ Agent 邮箱），按主题前缀路由到不同 AI Profile，自动回复。

## 模块结构

```
sdk/email-bridge/
  src/
    agently-mail.js     AgentlyMailClient — agently-cli subprocess 封装
    dispatcher.js       ProfileDispatcher — 路由 + 会话 + P0 执行
    index.js            包入口，导出 createEmailBridge / createProfile
    index.d.ts          TypeScript 类型定义
  profiles/
    _stream_json.js     stream-json CLI 共享 helper（claude/cursor/codebuddy）
    claude-code.js      Claude Code CLI (claude)
    cursor.js           Cursor Agent CLI (agent)
    codebuddy.js        CodeBuddy Code CLI (codebuddy)
    codex.js            OpenAI Codex CLI (codex)
    agy.js              Antigravity CLI (agy)
    echo.js             内置回显 Profile（调试用）
  bin/
    cli.js              ilink-email-bridge CLI 入口
  docs/
    ARCHITECTURE.md     架构方案与 Universal Agent Gateway 演化路径
  README.md             使用文档（快速开始、Profile 说明、防循环设计）
  email-profiles.example.yaml  配置文件示例
```

## 核心数据流

```
mail.listUnread()
    ↓ 过滤自发邮件（filterSelfSent）
    ↓ mail.read(message_id)
    ↓ cleanBody()  ← 去 HTML / 去 quoted 引用 / 去 Agently 签名 / 截断
    ↓ dispatcher.resolveProfile(subject)  ← 解析 [profile-name] 前缀
    ↓ _sessionId()  ← references[0] || in_reply_to || rfc_message_id
    ↓ loadHistory(sessionId)  ← 可选，ilink-bridge-profile 依赖
    ↓ _spawnProfile(P0 env vars)  ← 启动 Profile 子进程
    ↓ 解析 ILINK_SESSION / ILINK_PARTIAL / 响应文本
    ↓ appendHistory()
    ↓ mail.reply(message_id, response)
```

## 关键设计决策

### 1. 自发邮件过滤（`filterSelfSent`）

默认启用，跳过 `from.email == 我方邮箱` 的邮件，防止 Agent 回复自己的回复形成无限循环。
关闭：`createEmailBridge({ filterSelfSent: false })`

### 2. 线程 = 会话（`_sessionId`）

Session ID 取 `references[0]`（线程根 Message-ID），所有回复共享同一 AI 会话。
保证了"同一话题的多轮邮件 = 连续的对话上下文"。

### 3. 正文清理顺序（`cleanBody`）

1. HTML → Plain Text（保留换行结构）
2. 去除 Quoted 引用行（`>` 开头 + "On X wrote:" + 中文 headers）
3. 去除 Agently Mail 自动签名
4. 截断到 `maxBodyLength`（默认 8000 字）

### 4. Profile 执行（P0 协议）

所有 Profile 通过 4 个环境变量接收输入：
`ILINK_MESSAGE` / `ILINK_SESSION_ID` / `ILINK_SESSION_NAME` / `ILINK_FROM_USER`

Profile 通过 stdout 输出（可选）：
`ILINK_SESSION:<uuid>` + `ILINK_PARTIAL:<json>` + 响应文本

### 5. 内置 Profile 实现原则

- **stream-json 系列**（claude/cursor/codebuddy）：使用 `_stream_json.js` 共享 helper
- **JSONL 系列**（codex）：独立解析 `thread.started` / `item.completed` 事件
- **plain text 系列**（agy）：从 log 文件提取 conversation ID
- 所有内置 Profile 均实现 **session resume + 降级重试**

## 修改注意事项

- 修改 `dispatcher.js` 后需验证 `removeQuotedContent` / `cleanBody` / `resolveProfile` 逻辑
- 新增 Profile 时参考 `profiles/echo.js` 最小实现，确保 P0 协议兼容性
- 修改 `_parseSimpleYaml` 时注意行内注释（` # comment`）需被 `_stripInlineComment` 剥离
- `email-profiles.yaml` 的 `args` 路径支持相对路径（相对于 yaml 文件目录），会被 `_configDir` 解析

## 与 iLink Hub 的关系

| 组件 | iLink Hub | ilink-email-bridge |
|------|-----------|-------------------|
| 输入通道 | WeChat WebSocket | Email 轮询 |
| 路由层 | Bridge Manager (Rust) | ProfileDispatcher (Node.js) |
| 执行层 | Profile 子进程（P0） | 相同 |
| 会话存储 | SQLite/PostgreSQL | JSONL 文件（ilink-bridge-profile） |

详见 [架构文档](docs/ARCHITECTURE.md)。
