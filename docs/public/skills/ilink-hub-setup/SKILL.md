---
name: ilink-hub-setup
description: >-
  This skill should be used when the user wants to install ilink-hub, set up ilink-hub-bridge,
  configure a WeChat bot connection, or get started with the full iLink Hub system end-to-end.
  Triggers on: "安装 ilink-hub", "安装 bridge", "配置 ilink", "设置微信机器人", "接入 iLink",
  "ilink-hub 怎么装", "bridge 怎么配置", "如何开始使用 ilink", "setup ilink-hub", "install ilink-hub",
  "configure bridge", "get started with ilink", "ilink 新手入门", "帮我装一下 ilink".
version: 0.1.0
source: https://jeffkit.github.io/ilink-hub/skills/ilink-hub-setup/SKILL.md
---

# ilink-hub 安装与配置 Skill

本 skill 指导从零完成 iLink Hub 的安装、启动、微信绑定，以及 bridge 的配置与运行。

---

## 系统概述

```
微信用户 ←→ 微信 iLink ←→ ilink-hub（Hub 服务）←→ ilink-hub-bridge ←→ 本机 AI CLI
```

| 组件 | 作用 | 是否必须 |
|------|------|---------|
| `ilink-hub` | Hub 服务，与微信 iLink 通信，管理多客户端路由 | 必须（或用远程 Hub） |
| `ilink-hub-bridge` | 把微信消息转给本机 CLI（Claude Code 等） | 若用 bridge 模式则必须 |
| ClawBot（龙虾插件） | 微信端插件，iLink 协议来源 | 必须 |

---

## Step 1：前置条件确认

首先确认用户的情况，分两种路径：

- **路径 A（本机运行 Hub + Bridge）**：用户自己电脑运行完整服务
- **路径 B（远程 Hub + 本机 Bridge）**：Hub 已在服务器运行，仅配 bridge

询问用户：
1. 操作系统？（macOS / Linux / Windows）
2. Mac 芯片？（Apple Silicon M1/M2/M3/M4 还是 Intel）
3. Hub 是否已经在运行？若是，提供 Hub URL

然后确认用户微信是否已开启 **ClawBot（龙虾插件）**：  
微信 → 我 → 设置 → 插件 → 找到「龙虾插件」开启

---

## Step 2：安装 ilink-hub

根据操作系统选择安装方式：

### macOS（Homebrew，推荐）

```bash
# 若未安装 Homebrew：https://brew.sh
brew tap jeffkit/tap
brew install ilink-hub
```

安装后同时得到 `ilink-hub`、`ilink-hub-bridge`、`ilink-relay` 三个命令。

### macOS 直接下载

```bash
# Apple Silicon（M 系列芯片）
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-macos-aarch64
chmod +x ilink-hub ilink-hub-bridge
sudo mv ilink-hub ilink-hub-bridge /usr/local/bin/

# Intel Mac
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-x86_64
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-macos-x86_64
chmod +x ilink-hub ilink-hub-bridge
sudo mv ilink-hub ilink-hub-bridge /usr/local/bin/
```

### Linux x86_64

```bash
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
curl -Lo ilink-hub-bridge https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-bridge-linux-x86_64
chmod +x ilink-hub ilink-hub-bridge
sudo mv ilink-hub ilink-hub-bridge /usr/local/bin/
```

### 验证安装

```bash
ilink-hub --version
ilink-hub-bridge --version
```

---

## Step 3：启动 Hub（路径 A）

> 路径 B（远程 Hub 已运行）跳到 Step 4。

```bash
ilink-hub serve --addr 0.0.0.0:8765
```

**首次启动**会在终端显示二维码，用微信扫码完成绑定：

```
首次启动需要绑定微信机器人，请扫描下方二维码登录。
█████████████████████
...
```

扫码后，Hub 输出：
```
INFO ilink_hub: WeChat bot connected, token=wx_...
INFO ilink_hub: Listening on 0.0.0.0:8765
```

验证 Hub 正常：
```bash
curl http://127.0.0.1:8765/health
# 返回 {"status":"ok"}
```

### 后台持久运行（可选）

```bash
# macOS launchd（开机自启）或简单后台运行：
nohup ilink-hub serve --addr 0.0.0.0:8765 > ~/.ilink-hub/hub.log 2>&1 &
echo $! > ~/.ilink-hub/hub.pid
```

---

## Step 4：配置 Bridge

确认 Hub URL（本机默认 `http://127.0.0.1:8765`，远程则为对方地址）。

### 最简配置（接 Claude Code）

创建 `~/.ilink-hub-bridge/profiles/claude.yaml`：

```bash
mkdir -p ~/.ilink-hub-bridge/profiles
```

```yaml
# ~/.ilink-hub-bridge/profiles/claude.yaml
profiles:
  claude:
    type: claude-code
    cwd: /path/to/your/project   # ← 改为你的项目目录

routing:
  strategy: fixed
  default_profile: claude

skip_bot_messages: true
require_text: true
send_error_reply: true
```

> 接其他 AI CLI 或自定义 handler？使用 `/bridge-profile` skill 获取完整指引。

### 确保 Claude Code CLI 已就绪（若接 Claude Code）

```bash
# 检查 claude CLI
claude --version

# 尚未登录时
claude login
# 或设置 API Key
export ANTHROPIC_API_KEY="sk-ant-..."
```

---

## Step 5：启动 Bridge Manager

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
# 若 Hub 开了管理端鉴权（ILINK_ADMIN_TOKEN）：
# export ILINK_ADMIN_TOKEN=与 Hub 一致的值

ilink-hub-bridge manager
```

Manager 会自动扫描 `~/.ilink-hub-bridge/profiles/`，为每个 YAML 启动独立的 bridge 子进程，并向 Hub 自动注册。

成功输出示例：
```
INFO ilink_hub::bridge::manager: starting child bridge profile=claude ...
INFO ilink_hub::bridge: auto-registered client name=local-MacBook-claude token=vhub_...
INFO ilink_hub::bridge: connected to hub, polling for messages
```

---

## Step 6：微信中切换到 Bridge

在微信中发送 Hub 命令（与已绑定的微信机器人对话）：

```
/list
```

Hub 回复所有已注册客户端，找到你的 bridge（名如 `local-MacBook-claude`）：

```
/use local-MacBook-claude
```

此后发出的普通消息都会路由给该 bridge，转给 Claude Code 处理。

---

## 验证自测清单

完成以上步骤后逐项确认：

- [ ] `ilink-hub --version` 有输出
- [ ] `curl http://127.0.0.1:8765/health` 返回 `{"status":"ok"}`（或远程 Hub 可达）
- [ ] `ilink-hub-bridge --version` 有输出
- [ ] Manager 日志中出现 `connected to hub, polling for messages`
- [ ] 微信 `/list` 显示 bridge 客户端在线
- [ ] 微信发一条普通文字，本机 CLI 被触发，收到回复

---

## 常见问题排查

**Hub 无法启动 / 端口占用**
```bash
lsof -i :8765   # 查看占用端口的进程
```

**Bridge 注册失败（401）**
- Hub 开了 `ILINK_ADMIN_TOKEN`，需在 bridge 环境中导出相同值
```bash
export ILINK_ADMIN_TOKEN=与 Hub 配置一致的值
```

**Bridge 连不上 Hub**
```bash
curl $WEIXIN_BASE_URL/health   # 测试 Hub 是否可达
```

**claude CLI 未找到 / 认证失败**
```bash
which claude             # 确认在 PATH
claude login             # 重新登录
claude --version         # 验证版本
```

**Token 失效，bridge 自动重新注册但失败**
```bash
# 手动删除凭证，强制重注册
rm ~/.ilink-hub-bridge/credentials/claude-credentials.json
# 重启 manager
```

---

## 下一步

- 创建更多 profile（接不同 AI、不同项目）→ `/bridge-profile`
- 用 Python/Node.js SDK 开发自定义逻辑 → `/bridge-profile`
- 部署到服务器 → 参考 `docs/deployment/` 目录
