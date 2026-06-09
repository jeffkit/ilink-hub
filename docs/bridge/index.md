# ilink-hub-bridge

`ilink-hub-bridge` 是 iLink Hub 配套的**本地命令行后端**。

## 它是干什么的？

简单说：**微信发消息 → bridge 在你的电脑上执行一条命令 → 命令的输出回复到微信**。

```
微信用户发消息
    ↓
iLink Hub（消息路由）
    ↓
ilink-hub-bridge（本机运行）
    ↓ 执行你配置的命令，比如：
      claude --print "你的消息"
      echo "Hello"
      python my_bot.py
    ↓ 把 stdout 作为回复
微信用户收到回复
```

bridge 本身不内置任何 AI——你在 YAML 配置里写什么命令，它就执行什么命令。

---

## 适合什么场景？

| 场景 | 说明 |
|------|------|
| **微信对话 Claude Code** | 在微信里给本机的 Claude Code 下任务，让它帮你写代码、查文件 |
| **接入 Cursor / Codex** | 把 Cursor Agent 或 OpenAI Codex CLI 接到微信 |
| **快速测试** | 用 `echo` 命令验证整条链路是否通畅 |
| **自定义脚本** | 任何能在命令行运行、能输出文字的程序都能接入 |

---

## 与其他客户端的关系

bridge 和 Recursive、OpenClaw 一样，都是 Hub 的一个「后端客户端」——它们都通过 Hub 的 iLink 兼容 API 收发消息。区别只是：

- Recursive / OpenClaw：自带 AI 大模型
- **bridge**：不内置 AI，转而在本机执行你指定的命令

两者可以**同时注册**，用微信 `/use` 命令在它们之间切换。

---

## 快速导航

::: tip 第一次用？从这里开始
→ [**5 分钟上手：用 echo 走通完整链路**](./quick-try.md)
:::

- [**接入 Claude Code**](../guide/claude-code.md) — 微信直接对话 Claude Code 的完整教程
- [**使用指引（USAGE）**](./USAGE.md) — Claude Code、Cursor Agent、Codex、多项目配置
- [**功能与配置参考**](./README.md) — 所有 YAML 字段、命令行参数、环境变量
- [**开发自定义 Profile**](./profile-spec.md) — 用 Node.js / Python 实现自己的处理器

---

## 安装

bridge 和 Hub 主程序一起分发，装一次即可：

::: code-group

```bash [macOS（Homebrew）]
brew tap jeffkit/tap
brew install ilink-hub
# ilink-hub 和 ilink-hub-bridge 都装好了
```

```bash [macOS Apple Silicon（直接下载）]
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-macos-aarch64
chmod +x ilink-hub-bridge && sudo mv ilink-hub-bridge /usr/local/bin/
```

```bash [Linux x86_64]
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-linux-x86_64
chmod +x ilink-hub-bridge && sudo mv ilink-hub-bridge /usr/local/bin/
```

:::

验证：

```bash
ilink-hub-bridge --version
```
