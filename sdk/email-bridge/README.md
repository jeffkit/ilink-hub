# ilink-email-bridge

> iLink Hub 邮件通道适配器：轮询 Agently Mail 邮箱，按主题前缀路由到多个 AI Profile，自动回复。

## 概念

```
[用户/其他 Agent 发邮件]
         ↓
[ilink-email-bridge 定时轮询]
         ↓ 解析 [前缀] 路由
[ProfileDispatcher]   ← email-profiles.yaml
         ↓ P0 协议 spawn
[Profile 子进程]     ← 任何现有 ilink-bridge-profile 兼容脚本
         ↓ 读取响应
[agently-cli +reply 回复邮件]
```

邮件通道与微信通道是**对称的**：
- 触发方式不同（邮件轮询 vs 微信 WebSocket push）
- 但后端 Profile 完全相同，无需修改

---

## 快速开始

### 1. 前置条件

```bash
# 安装 agently-cli
npm install -g @tencent-qqmail/agently-cli

# 登录授权（会打开浏览器）
agently-cli auth login

# 验证
agently-cli +me
```

### 2. 安装本包

```bash
npm install ilink-email-bridge
```

或在 monorepo 中直接引用：

```bash
# 从 ilink-hub 仓库根目录
node sdk/email-bridge/bin/cli.js --config examples/email-bridge/email-profiles.yaml
```

### 3. 配置 email-profiles.yaml

```yaml
# 没有匹配到前缀时使用的默认 Profile
default: claude

profiles:
  claude:
    command: node
    args:
      - ./profiles/claude-handler.js
    description: Claude Code 默认助手
    trigger: claude       # 匹配主题前缀 [claude]

  cursor:
    command: node
    args:
      - ./profiles/cursor-handler.js
    description: Cursor AI 编程助手
    trigger: cursor       # 匹配主题前缀 [cursor]

  echo:
    command: node
    args:
      - ./profiles/echo.js
    description: 原样回显（调试用）
    trigger: echo
```

### 4. 启动

```bash
# 生产：每 5 分钟轮询
ilink-email-bridge --config ./email-profiles.yaml

# 调试：30 秒轮询，不实际发送邮件
ilink-email-bridge --config ./email-profiles.yaml --interval 30000 --dry-run
```

---

## 如何向 Agent 发邮件

| 场景 | 主题格式 | 路由结果 |
|------|---------|---------|
| 默认 AI 处理 | `帮我分析这份报告` | → default profile |
| 指定 Claude | `[claude] 解释这段代码` | → claude profile |
| 指定 Cursor | `[cursor] 帮我 review PR` | → cursor profile |
| 调试测试 | `[echo] 你好` | → echo profile（回显） |

**主题前缀格式**：`[profile-name] 实际主题`

---

## Profile 兼容性

任何符合 [ilink-bridge-profile P0 协议](https://github.com/youorg/ilink-hub/tree/main/sdk/node) 的脚本都可以直接接入，无需修改。

P0 协议约定：

```
Input  (env vars):
  ILINK_MESSAGE      邮件正文（已清理：剥离 HTML、移除 quoted 引用、截断超长内容）
  ILINK_SESSION_ID   同一邮件线程的历史 Session ID（可为空）
  ILINK_SESSION_NAME 格式为 "email-发件人邮箱"
  ILINK_FROM_USER    发件人邮箱地址

Output (stdout):
  ILINK_SESSION:<uuid>    可选，更新 Session ID
  ILINK_PARTIAL:<json>    可选，流式输出块
  <回复内容>              最终回复文本
```

---

## 内置 Profiles

包内置了所有主流 Coding Agent CLI 的 Node.js Profile，无需额外安装：

| Profile 文件 | 调用的 CLI | 主题前缀 | 所需 CLI |
|-------------|-----------|---------|---------|
| `profiles/claude-code.js` | `claude` | `[claude]` | Claude Code |
| `profiles/cursor.js` | `agent` | `[cursor]` | Cursor Agent |
| `profiles/codebuddy.js` | `codebuddy` | `[codebuddy]` | CodeBuddy Code |
| `profiles/codex.js` | `codex` | `[codex]` | OpenAI Codex |
| `profiles/agy.js` | `agy` | `[agy]` | Antigravity (DeepMind) |
| `profiles/echo.js` | （内置） | `[echo]` | 无（调试用） |

所有 Profiles 都实现了：
- **session 续传**：`--resume <uuid>` 恢复上次对话
- **session 降级**：session 失效时自动重试新会话
- **P0 兼容**：`ILINK_PARTIAL` 流式输出 + `ILINK_SESSION` 更新

---

## 邮件线程与会话

每个「邮件线程 × Profile」对应一个独立的 AI 会话（Session）。

```
原始邮件 A (rfc_message_id: <msgA>)
  └── 你回复 → 已过滤（自发）
       └── 用户再次回复 B (in_reply_to: <msgA>, references: [<msgA>])
            └── 共享同一 session_id: email_claude_msgA_xxx
```

**Session key 计算规则**（按优先级）：
1. `references[0]`（RFC 2822 规范的线程根 Message-ID）
2. `in_reply_to`（直接父级，适用于只有一级回复的情况）
3. `rfc_message_id`（全新线程，从这封邮件开始）

效果：**同一条邮件链的所有消息共享一个 AI 会话**，Profile 能记住整个对话上下文，无需用户重复说明背景。

---

## 防循环设计

默认开启 **自发邮件过滤**：跳过所有 `from.email` 与自己邮箱地址一致的邮件，防止 Agent 对自己的回复再次处理，形成无限循环。

```js
createEmailBridge({
  filterSelfSent: true,   // 默认 true，设为 false 可关闭
});
```

---

## 正文清理流程

收到邮件后，在传给 Profile 前会做三步清理，减少 token 消耗和噪音：

1. **HTML → Plain Text**：剥离所有 HTML 标签，转换换行元素
2. **移除 Quoted 引用**：过滤掉以 `>` 开头的引用行、"On [date] wrote:" 分隔符、中文邮件的引用头部（发件人/主题/发送时间块）
3. **截断超长正文**：默认 8000 字符，超出后附说明（可通过 `ProfileDispatcher` 选项调整）

---

## HTML 邮件格式支持

Agent 回复**自动使用 HTML 格式**，将 AI 输出的 Markdown 转换为格式化的 HTML 邮件，提供更好的阅读体验。

### 支持的 Markdown 特性

- ✅ **标题**：`#`、`##`、`###` 等
- ✅ **加粗/斜体**：`**加粗**`、`*斜体*`
- ✅ **代码**：行内 `` `code` ``、代码块 ` ```language `
- ✅ **列表**：有序列表、无序列表、嵌套列表
- ✅ **引用**：`> quote`
- ✅ **链接**：`[text](url)`
- ✅ **表格**：Markdown 表格（GFM）
- ✅ **分隔线**：`---` 或 `***`

### 样式设计

使用 GitHub 风格样式，包含：
- 清晰的标题层级（带下划线）
- 代码块背景高亮
- 表格斑马条纹
- 适中的行距和间距
- 响应式设计（最大宽度 800px）

### 程序化使用

```js
const { convertMarkdownToHtml } = require('ilink-email-bridge');

const markdown = `
## 你好

这是 **加粗** 文字，还有 \`代码\`。
`;

const html = convertMarkdownToHtml(markdown);
// → 完整的 HTML 文档，包含样式
```

---

## 程序化使用

```js
const {
  createEmailBridge,
  AgentlyMailClient,
  ProfileDispatcher,
} = require('ilink-email-bridge');

// 最简启动
createEmailBridge({
  profilesConfig: './email-profiles.yaml',
  pollIntervalMs: 5 * 60_000,
});

// 高级：手动控制
const mail = new AgentlyMailClient();
const dispatcher = new ProfileDispatcher('./email-profiles.yaml');
const { convertMarkdownToHtml } = require('ilink-email-bridge');

const poller = mail.poll(60_000, async (msg, client) => {
  const full = client.read(msg.message_id);
  const { response, profileName } = dispatcher.dispatch(full);
  console.log(`[${profileName}] → ${response.length} chars`);
  
  // 将 Markdown 响应转换为 HTML 邮件
  const htmlResponse = convertMarkdownToHtml(response);
  client.reply(msg.message_id, htmlResponse, { bodyFormat: 'html' });
}, { limit: 10 });

// 优雅关闭
process.on('SIGINT', () => poller.stop());
```

---

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `POLL_INTERVAL_MS` | 轮询间隔（毫秒） | `300000`（5 分钟） |
| `PROFILES_CONFIG` | profiles 配置文件路径 | `./email-profiles.yaml` |
| `DRY_RUN` | 设为 `1` 时不实际发邮件 | — |

---

## 速率限制参考

Agently Mail 默认限额（每账户）：

| 限制 | 值 |
|------|-----|
| 每日发送 | 50 封 |
| 每小时请求 | 200 次 |
| 每分钟请求 | 10 次 |

5 分钟轮询 = 288 次检查/天，远低于限额。即使每次轮询都有邮件要处理，每天最多处理 50 封（受发送限额约束）。

---

## 架构关系

```
iLink Hub 架构 (微信通道)      Email Bridge (邮件通道)
─────────────────────────      ──────────────────────────
ilink-hub (Rust)               ilink-email-bridge (Node.js)
  ↑ 接收微信消息                  ↑ 轮询 agently-cli
  ↓ 路由到 Bridge                 ↓ ProfileDispatcher 路由
Bridge Manager                 ProfileDispatcher
  ↓ spawn Profile                 ↓ spawn Profile (P0)
Profile 子进程      ←────────── 同一套 Profile 脚本
```

两个通道共享同一套 Profile 脚本，代码零重复。

---

## 开发

```bash
# 调试（30 秒轮询，dry-run）
POLL_INTERVAL_MS=30000 DRY_RUN=1 node bin/cli.js --config ./email-profiles.yaml

# 测试 SDK
node -e "
const { AgentlyMailClient } = require('./src/index');
const mail = new AgentlyMailClient();
console.log(mail.me());
"
```
