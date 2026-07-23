# 接入 Claude Code

本页带你完成 **安装并启动 Hub → 在 im-agentproc 侧配置 Bridge → 用微信和 Claude Code 对话** 的完整流程。

> **Bridge 已独立**：本地 CLI bridge（原 `ilink-hub-bridge`）已从本仓库拆分到独立项目
> [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)（crate `im-agentproc`，
> bin `im-agentproc`）。本页只覆盖 **Hub 侧** 的安装与启动；bridge 的安装、profile 配置、
> 启动命令请到 im-agentproc 仓库查阅。

---

## 你需要准备

| 条件 | 说明 |
|------|------|
| 微信中已开启 ClawBot（龙虾插件） | 更新微信到最新版 → 「我 → 设置 → 插件」开启，无需申请审核 |
| macOS 或 Linux 电脑 | Windows 可用但部分步骤略有不同，见下方说明 |
| Node.js 18 或更高版本 | 终端运行 `node --version` 检查；没有的话去 [nodejs.org](https://nodejs.org) 下载 LTS 版 |
| Anthropic API Key 或 Claude 账号 | 在 [console.anthropic.com](https://console.anthropic.com) 获取 |

---

## 第一步：安装 iLink Hub

::: code-group

```bash [macOS (Homebrew)]
brew tap jeffkit/tap
brew install ilink-hub
```

```bash [macOS Apple Silicon（直接下载）]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

```bash [Linux x86_64]
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

:::

验证：

```bash
ilink-hub --version
```

> **Windows 用户**：从 [Releases 页面](https://github.com/jeffkit/ilink-hub/releases/latest) 下载 `ilink-hub-windows-x86_64.exe`，加入 PATH 并去掉 `.exe` 后缀使用。
>
> 注：自 `0.4.0` 起，Releases 不再附带 `ilink-hub-bridge-*` 资产；bridge 改由
> [im-agentproc](https://github.com/jeffkit/im-agentproc) 独立发布。

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

## 第四步：在 im-agentproc 侧配置并启动 Bridge

Hub 侧准备好后，前往独立项目 **[jeffkit/im-agentproc](https://github.com/jeffkit/im-agentproc)**：

1. 安装 `im-agentproc`（原 `ilink-hub-bridge`）。
2. 创建一个 profile YAML，`executor: claude-code`，`cwd` 指向你的项目目录。
3. 启动 bridge，`WEIXIN_BASE_URL=http://127.0.0.1:8765`，向 Hub 自动注册拿到 `vhub_…`。
4. 在微信里 `/use <注册名>` 切到该后端。

具体命令、YAML 字段、示例请以 im-agentproc 仓库文档为准。

---

## 第五步：测试

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

如果 Hub 启动时设置了管理 Token，bridge 侧也要设置相同的值：

```bash
export ILINK_ADMIN_TOKEN=与_Hub_一致的值
# 再启动 im-agentproc bridge
```

**收不到消息**

在微信发 `/list`，确认你的 bridge 客户端显示在线。如果有多个客户端，用 `/use <名称>` 切换。

**Claude 回复很慢**

正常现象，Claude Code 复杂任务可能需要 30–120 秒。可在 bridge profile 里适当增大 `timeout_secs`。

**想换一个项目目录**

修改 im-agentproc 的 profile YAML 里的 `cwd`，重启 bridge 即可。

---

## 下一步

- [Bridge 项目 im-agentproc](https://github.com/jeffkit/im-agentproc) — 安装、profile 配置、多 CLI / 多项目使用
- [微信命令](/reference/commands) — `/list`、`/use`、`/session`、`/broadcast` 等
