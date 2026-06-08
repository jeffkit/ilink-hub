# 用 Node.js 开发 Bridge Profile

> 最后更新：2026-06-08

本教程带你从零开始，用 Node.js 编写一个能接收微信消息、调用 AI API、返回回复的 Bridge Profile，并将它接入 iLink Hub Bridge。

---

## 前置要求

- Node.js 18+
- 已安装并运行 `ilink-hub`（参见[快速开始](/guide/getting-started)）
- 已安装 `ilink-hub-bridge`（`brew install jeffkit/tap/ilink-hub`）

---

## 第一步：创建项目

```bash
mkdir my-ai-profile && cd my-ai-profile
npm init -y
npm install ilink-bridge-profile
```

---

## 第二步：编写 handler

创建 `handler.js`：

```js
const { createProfile } = require('ilink-bridge-profile');

createProfile(async ({ message, sessionId, fromUser }) => {
  // 这里写你的 AI 调用逻辑
  // 示例：简单的 echo 回复
  return `你说的是：${message}`;
});
```

`createProfile` 帮你做了所有样板工作：
- 读取 `ILINK_MESSAGE`、`ILINK_SESSION_ID` 等环境变量
- 调用你的 handler 函数
- 按 P0 协议把回复写到 stdout

### 调用真实 AI（以 OpenAI 为例）

```bash
npm install openai
```

```js
const { createProfile } = require('ilink-bridge-profile');
const OpenAI = require('openai');

const client = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });

createProfile(async ({ message }) => {
  const completion = await client.chat.completions.create({
    model: 'gpt-4o-mini',
    messages: [{ role: 'user', content: message }],
  });
  return completion.choices[0].message.content;
});
```

---

## 第三步：本地测试

不需要启动完整的 bridge，直接用环境变量模拟调用：

```bash
ILINK_MESSAGE="你好，介绍一下自己" \
ILINK_SESSION_ID="" \
ILINK_FROM_USER="test-user" \
node handler.js
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
    script: ./handler.js    # bridge 自动用 node 运行
    timeout_secs: 60

routing:
  default_profile: my-ai
```

启动 bridge：

```bash
ilink-hub-bridge --config profiles.yaml
```

现在发微信消息给你的 ClawBot，就会触发 `handler.js`！

---

## 第五步：支持多轮对话

如果你直接调用 LLM API（不用 Claude Code），可以用 SDK 的历史管理功能保存上下文：

```js
const { createProfile, loadHistory, appendHistory } = require('ilink-bridge-profile');
const OpenAI = require('openai');

const client = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });

createProfile(async ({ message, sessionId }) => {
  // 读取历史对话
  const history = loadHistory(sessionId);
  const messages = [
    { role: 'system', content: '你是一个友好的 AI 助手。' },
    ...history.map(e => ({ role: e.role, content: e.content })),
    { role: 'user', content: message },
  ];

  const completion = await client.chat.completions.create({
    model: 'gpt-4o-mini',
    messages,
  });
  const reply = completion.choices[0].message.content;

  // 写入历史，下次对话时使用
  appendHistory(sessionId, [
    { role: 'user', content: message },
    { role: 'assistant', content: reply },
  ]);

  return { response: reply, sessionId };
});
```

历史文件保存在 `~/.ilink-hub/sessions/<session_id>.jsonl`，自动按会话隔离。

---

## 完整示例：接入 Gemini

```bash
npm install @google/generative-ai
```

```js
// gemini-profile.js
const { createProfile } = require('ilink-bridge-profile');
const { GoogleGenerativeAI } = require('@google/generative-ai');

const genAI = new GoogleGenerativeAI(process.env.GEMINI_API_KEY);

createProfile(async ({ message }) => {
  const model = genAI.getGenerativeModel({ model: 'gemini-pro' });
  const result = await model.generateContent(message);
  return result.response.text();
});
```

`profiles.yaml`：

```yaml
profiles:
  gemini:
    script: ./gemini-profile.js
    env:
      GEMINI_API_KEY: "your-api-key-here"
    timeout_secs: 30

routing:
  default_profile: gemini
```

---

## 发布为 npm 包（可选）

如果你想分享 profile 给其他人：

```bash
# 把 handler.js 变成可执行的 CLI
chmod +x handler.js
```

在 `package.json` 中添加：

```json
{
  "bin": {
    "my-ai-profile": "./handler.js"
  }
}
```

发布：

```bash
npm publish
```

其他用户安装后，在 YAML 中用 `command` 引用：

```yaml
profiles:
  my-ai:
    command: my-ai-profile
```

---

## 下一步

- [P0 协议规范](/bridge/profile-spec) — 完整技术规范
- [Python 版本教程](/bridge/develop-python) — 用 Python 编写同等功能的 Profile
- [接入 Claude Code](/guide/claude-code) — 使用内置 claude-code profile
