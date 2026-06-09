# 接入 Claude Code

本页从零到一，带你完成 **安装 → 启动 Hub → 配置 Bridge → 用微信和 Claude Code 对话** 的完整流程。

---

## 你需要准备

| 条件 | 说明 |
|------|------|
| 已开通 iLink（ClawBot）的微信账号 | 在 [ilinkai.weixin.qq.com](https://ilinkai.weixin.qq.com) 申请，1-3 个工作日审核 |
| macOS 或 Linux 电脑 | Windows 可用但部分步骤略有不同，见下方说明 |
| Node.js 18 或更高版本 | 终端运行 `node --version` 检查；没有的话去 [nodejs.org](https://nodejs.org) 下载 LTS 版 |
| Anthropic API Key 或 Claude 账号 | 在 [console.anthropic.com](https://console.anthropic.com) 获取 |

---

## 第一步：安装 iLink Hub 和 Bridge

两个二进制都在同一个包里，装一次即可。

::: code-group

```bash [macOS (Homebrew)]
brew tap jeffkit/tap
brew install ilink-hub
```

```bash [macOS Apple Silicon（直接下载）]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-macos-aarch64
chmod +x ilink-hub ilink-hub-bridge
sudo mv ilink-hub ilink-hub-bridge /usr/local/bin/
```

```bash [Linux x86_64]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-linux-x86_64
chmod +x ilink-hub ilink-hub-bridge
sudo mv ilink-hub ilink-hub-bridge /usr/local/bin/
```

:::

验证：

```bash
ilink-hub --version
ilink-hub-bridge --version
```

> **Windows 用户**：从 [Releases 页面](https://github.com/jeffkit/ilink-hub/releases/latest) 下载 `ilink-hub-windows-x86_64.exe` 和 `ilink-hub-bridge-windows-x86_64.exe`，加入 PATH 并去掉 `.exe` 后缀使用。

---

## 第二步：安装 Claude Code CLI

```bash
npm install -g @anthropic-ai/claude-code
```

验证并登录：

```bash
claude --version

# 选择一种登录方式：
claude login                         # OAuth 登录（推荐）
# 或
export ANTHROPIC_API_KEY=sk-ant-...  # 直接用 API Key
```

---

## 第三步：启动 Hub（首次会出二维码登录）

**新开一个终端**，保持它一直运行：

```bash
ilink-hub serve --addr 127.0.0.1:8765
```

首次启动会在终端打印二维码，用**已开通 iLink 的微信**扫码。扫码成功后看到：

```
iLink login successful, token saved
INFO ilink_hub: iLink Hub listening on 127.0.0.1:8765
```

::: tip
可选：打开 [http://127.0.0.1:8765/hub/ui](http://127.0.0.1:8765/hub/ui) 确认 Hub 状态。
:::

---

## 第四步：创建 Bridge 配置文件

新建 `~/ilink-claude.yaml`，把 `cwd` 改为你的项目目录：

```yaml
# ~/ilink-claude.yaml
profiles:
  claude:
    type: claude-code            # 内置处理器：自动管理 --resume、session 追踪
    cwd: ~/your-project          # ← 改为你的项目目录（Claude 会在这里读写文件）
    timeout_secs: 300

  claude_new:                    # 可选：/new 前缀强制开新对话
    type: claude-code
    cwd: ~/your-project
    env:
      ILINK_SESSION_ID: ""       # 强制新会话

routing:
  strategy: prefix
  default_profile: claude
  prefix_rules:
    - prefix: "/new "
      profile: claude_new
```

::: tip 没有特定项目？
`cwd` 设为任意目录都行，比如 `~`。Claude Code 会在该目录下工作。
:::

---

## 第五步：启动 Bridge

**再开一个新终端**：

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
ilink-hub-bridge --config ~/ilink-claude.yaml
```

首次运行会自动向 Hub 注册，终端显示类似：

```
✓ 已自动注册客户端 local-xxxx
  → 在微信发送 /use local-xxxx 切换到该客户端
INFO bridge: waiting for messages…
```

在微信里发送提示里的 `/use local-xxxx` 命令（如果 Hub 上只有这一个客户端，这步可以跳过）。

---

## 第六步：测试

### 先验证 Bridge 本身能调通 Claude

```bash
ILINK_MESSAGE="用一句话介绍你自己" \
ILINK_SESSION_ID="" \
ilink-hub-bridge profile claude-code
```

看到 Claude 的回复就说明链路通了。

### 然后在微信里发消息

在微信里直接发：

```
你好，帮我用 Python 写一个 Hello World
```

预期：你会收到 Claude Code 的回复，并且下一条消息会自动保持上下文（`--resume`）。

---

## 日常使用：Session 管理

Hub 内建 session 管理，可以在同一个微信对话里维护多个独立的 Claude 上下文（比如不同项目、不同功能分支）：

| 命令 | 说明 |
|------|------|
| `/session list` | 列出所有 sessions |
| `/session new feat-login` | 新建名为 `feat-login` 的 session |
| `/session use feat-login` | 切换到 `feat-login`（后续消息用该 session resume）|
| `/session delete feat-login` | 删除 session |
| `/new 你的问题` | 临时强制新会话，不影响已有 sessions |

---

## 常见问题

**自动注册失败（401）**
```bash
# 如果 Hub 启动时设置了管理 Token，Bridge 也要设置相同的值：
export ILINK_ADMIN_TOKEN=与_Hub_一致的值
ilink-hub-bridge --config ~/ilink-claude.yaml
```

**收不到消息**

在微信发 `/list`，确认你的 bridge 客户端显示在线。如果有多个客户端，用 `/use <名称>` 切换。

**Claude 回复很慢**

正常现象，Claude Code 复杂任务可能需要 30–120 秒。可适当增大 `timeout_secs`。

**想换一个项目目录**

修改 `~/ilink-claude.yaml` 里的 `cwd`，重启 bridge 即可。

---

## 下一步

- [管理多个 Claude Session](/bridge/USAGE#claude-code) — 多任务并行
- [接入其他 CLI（Cursor、Codex）](/bridge/USAGE) — 多工具切换
- [开发自定义 Profile](/bridge/profile-spec) — 接入任意 AI 服务
