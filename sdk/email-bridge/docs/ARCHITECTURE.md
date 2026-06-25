# Agent Gateway 架构方案

## 一句话概括

iLink Hub 和 ilink-email-bridge 都是 **Universal Agent Gateway** 的通道实例——它们共享同一个执行协议（P0），只是触发方式不同。

---

## 问题背景

AI Agent 需要与人类和其他 Agent 通信。目前常见的接入方式：

- **微信**：实时、亲密、移动优先；但需要账号绑定、只有人类用户可达
- **邮件**：异步、通用、机器友好；但传统工具链繁重、无结构化路由
- **API**：精准、高效；但需要双方约定协议、开发成本高

每种方式都有其独特场景。**问题不是选哪个，而是如何让它们共用同一套 Agent 执行能力。**

---

## 核心抽象：P0 协议

iLink Hub 定义了一个极简的 **P0（Profile Zero）协议**，将 Agent 执行单元与通信通道完全解耦：

```
输入（环境变量）:
  ILINK_MESSAGE        用户消息
  ILINK_SESSION_ID     续传 Session UUID（空 = 新会话）
  ILINK_SESSION_NAME   会话标识名（如 "email-user@example.com"）
  ILINK_FROM_USER      发送方标识

输出（stdout）:
  ILINK_SESSION:<uuid>   可选，更新 Session ID
  ILINK_PARTIAL:<json>   可选，流式片段
  <response text>         最终响应文本
```

这是一个只有 4 个环境变量的接口，任何能读写环境变量和 stdout 的程序都可以成为 Profile。

---

## 架构图：Universal Agent Gateway

```
┌─────────────────────────────────────────────────────────┐
│                 Universal Agent Gateway                  │
│                                                         │
│  Input Channels (Adapters)                              │
│  ┌────────────┐  ┌─────────────┐  ┌──────────────────┐ │
│  │WeChat      │  │Email        │  │HTTP Webhook      │ │
│  │(Rust/WS)   │  │(Node.js/    │  │(REST, 待做)      │ │
│  │ilink-hub   │  │ polling)    │  │                  │ │
│  └─────┬──────┘  └──────┬──────┘  └────────┬─────────┘ │
│        │                │                   │            │
│        └────────────────┼───────────────────┘            │
│                         ↓                                 │
│              ┌──────────────────────┐                    │
│              │  Profile Dispatcher  │                    │
│              │  (路由 + 会话持久化)  │                    │
│              └──────────┬───────────┘                    │
│                         │  P0 协议                        │
│                         ↓                                 │
│  Profile Pool (shared across all channels)               │
│  ┌───────────┐ ┌────────┐ ┌──────────┐ ┌─────────────┐ │
│  │claude-code│ │cursor  │ │codebuddy │ │  自定义 ...  │ │
│  └───────────┘ └────────┘ └──────────┘ └─────────────┘ │
└─────────────────────────────────────────────────────────┘
```

---

## 两个通道的对比

| 维度 | WeChat (ilink-hub) | Email (ilink-email-bridge) |
|------|-------------------|---------------------------|
| 协议 | WebSocket 长连接 | SMTP/POP3，定时轮询 |
| 实时性 | 秒级响应 | 分钟级（可调） |
| 发起方 | 人类用户 | 人类用户 / 其他 Agent |
| 会话载体 | 微信线程 ID | 邮件 References 头部 |
| 接入成本 | 需绑定微信账号 | 只需 QQ Agent 邮箱 |
| 路由机制 | `/use cursor` 命令 | 主题前缀 `[cursor]` |
| 最适场景 | 日常对话、即时问答 | 任务委托、Agent 协作、异步处理 |

**关键洞察**：两者的差异只在触发层，执行层（Profile）完全共享。

---

## Agent-to-Agent 通信

Email Bridge 最独特的价值：**它是 Agent-to-Agent 通信最自然的协议**。

```
Agent A (任意 AI 系统)
    ↓ 发邮件
    Subject: [cursor] 请帮我 review 这个 PR
    To: jeffkit4781@agent.qq.com

Email Bridge (ilink-email-bridge)
    ↓ 轮询检测 → 路由到 cursor Profile
    ↓ 执行 Cursor CLI

Agent B (Agent A 的执行代理)
    ↓ 回复邮件 (Re: [cursor] ...)
    → Agent A 收到响应
```

无需约定 SDK、无需专属 API、无需注册账号——标准邮件协议就是接口。

---

## 演化路径

### 当前阶段（已完成）

```
ilink-hub (WeChat)    ilink-email-bridge (Email)
       ↓                        ↓
  P0 协议              P0 协议（相同）
       ↓                        ↓
  Profile Pool        Profile Pool（相同脚本）
```

两个独立部署的服务，共用 Profile 代码，但没有统一的调度层。

### 中期：提炼 gateway-core

```
@ilink/gateway-core
  ├── createChannel(adapter, profilesConfig)  // 3 行接入新通道
  ├── ProfileDispatcher                       // 共享路由逻辑
  ├── SessionStore                            // 跨通道会话共享
  └── P0Runner                               // Profile 执行
```

任何人可以用 3 行代码将新的通信通道接入 Agent Gateway：

```js
import { createChannel } from '@ilink/gateway-core';

createChannel({
  // 输入适配器：任意异步消息源
  poll: async () => slack.getUnreadMessages(),
  reply: async (msg, response) => slack.reply(msg.ts, response),
  profilesConfig: './profiles.yaml',
});
```

### 长期：跨系统 Agent 网络

当多个组织都运行各自的 Agent Gateway 时：

```
你的 Agent Gateway              其他 Agent Gateway
  [cursor]@jeffkit.ai    ←→     [claude]@partner.company.com

通信协议：邮件（RFC 5322）
路由协议：Subject 前缀 [profile-name]
会话协议：In-Reply-To / References
```

形成一个去中心化、标准协议的 Agent 协作网络，无需任何中央基础设施。

---

## 设计原则

1. **通道是插件，不是核心** — 任何通信方式都可以作为输入适配器接入
2. **P0 是唯一合约** — Profile 开发者只需关心 env vars in / stdout out
3. **会话跨通道共享** — 同一个 Session ID 无论从哪个通道进来都能延续
4. **渐进式复杂度** — 从一个简单的 echo profile 开始，逐步扩展到 AI CLI
5. **异步优先** — 邮件的异步特性是优势，不是缺陷；Agent 不应假设实时响应

---

## 与现有系统的关系

```
iLink Hub (Rust 核心)
  - 高性能、低延迟的 WeChat 通道
  - 内置 claude-code / cursor / codex / codebuddy / agy 等 Profile
  - 生产级会话持久化（SQLite/PostgreSQL）

ilink-email-bridge (Node.js 通道)
  - Email 通道适配器（非替代品，是补充）
  - 与 iLink Hub 共享 Profile 执行层
  - 独立部署，无运行时依赖
  - 可作为未来 @ilink/gateway-core 的第一个 Node.js 通道参考实现
```

---

## 附录：当前实现清单

| 组件 | 位置 | 状态 |
|------|------|------|
| AgentlyMailClient | `sdk/email-bridge/src/agently-mail.js` | ✅ 完成 |
| ProfileDispatcher | `sdk/email-bridge/src/dispatcher.js` | ✅ 完成 |
| createEmailBridge | `sdk/email-bridge/src/index.js` | ✅ 完成 |
| 内置 claude-code Profile | `sdk/email-bridge/profiles/claude-code.js` | ✅ 完成 |
| 内置 cursor Profile | `sdk/email-bridge/profiles/cursor.js` | ✅ 完成 |
| 内置 codebuddy Profile | `sdk/email-bridge/profiles/codebuddy.js` | ✅ 完成 |
| 内置 codex Profile | `sdk/email-bridge/profiles/codex.js` | ✅ 完成 |
| 内置 agy Profile | `sdk/email-bridge/profiles/agy.js` | ✅ 完成 |
| 自发邮件过滤 | `src/index.js: filterSelfSent` | ✅ 完成 |
| 邮件线程 → 共享 Session | `dispatcher.js: _sessionId` | ✅ 完成 |
| HTML/quoted/footer 清理 | `dispatcher.js: cleanBody` | ✅ 完成 |
| CLI 工具 | `sdk/email-bridge/bin/cli.js` | ✅ 完成 |
| @ilink/gateway-core 抽象 | — | 🔲 待做 |
| Slack 通道适配器 | — | 🔲 待做 |
| REST Webhook 通道适配器 | — | 🔲 待做 |
