---
name: ilink-hub-setup
description: >-
  This skill should be used when the user wants to install ilink-hub (the Hub service),
  configure a WeChat bot connection, or get started with iLink Hub end-to-end.
  The bridge (formerly ilink-hub-bridge) now lives in a separate project, im-agentproc;
  this skill covers the Hub side and points to im-agentproc for bridge setup.
  Triggers on: "安装 ilink-hub", "配置 ilink", "设置微信机器人", "接入 iLink",
  "ilink-hub 怎么装", "如何开始使用 ilink", "setup ilink-hub", "install ilink-hub",
  "get started with ilink", "ilink 新手入门", "帮我装一下 ilink".
version: 0.4.0
source: https://jeffkit.github.io/ilink-hub/skills/ilink-hub-setup/SKILL.md
---

# ilink-hub 安装与配置 Skill

本 skill 指导从零完成 **iLink Hub（Hub 服务本体）** 的安装、启动与微信绑定。

> **Bridge 已独立**：本地 CLI bridge（原 `ilink-hub-bridge`）已拆分到独立项目
> [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)（crate `im-agentproc`，
> bin `im-agentproc`）。如需「微信消息 → 本机 CLI（Claude Code 等）」，安装与配置请到
> im-agentproc 仓库；本 skill 只覆盖 Hub 侧。

---

## 系统概述

```
微信用户 ←→ 微信 iLink ←→ ilink-hub（Hub 服务）←→ 多个 AI 后端客户端
```

| 组件 | 作用 | 是否必须 |
|------|------|---------|
| `ilink-hub` | Hub 服务，与微信 iLink 通信，管理多客户端路由 | 必须（或连远程 Hub） |
| ClawBot（龙虾插件） | 微信端插件，iLink 协议来源 | 必须 |
| `im-agentproc`（原 `ilink-hub-bridge`） | 把微信消息转给本机 CLI | 可选，独立项目 |

---

## Step 1：前置条件确认

询问用户：
1. 操作系统？（macOS / Linux / Windows）
2. Mac 芯片？（Apple Silicon M1/M2/M3/M4 还是 Intel）
3. Hub 是本机运行，还是连远程已有的 Hub？若远程，提供 Hub URL。

然后确认用户微信是否已开启 **ClawBot（龙虾插件）**：
微信 → 我 → 设置 → 插件 → 找到「龙虾插件」开启

---

## Step 2：安装 ilink-hub

### macOS（Homebrew，推荐）

```bash
# 若未安装 Homebrew：https://brew.sh
brew tap jeffkit/tap
brew install ilink-hub
```

> 自 `0.4.0` 起，`jeffkit/tap/ilink-hub` formula 仅安装 `ilink-hub`（Hub 服务本体），
> 不再附带 `ilink-hub-bridge`。bridge 由 im-agentproc 独立 formula 提供。

### macOS / Linux 直接下载

```bash
# Apple Silicon（M 系列芯片）
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/

# Intel Mac
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/

# Linux x86_64
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

> Windows：从 [Releases](https://github.com/jeffkit/ilink-hub/releases/latest) 下载
> `ilink-hub-windows-x86_64.exe`，加入 PATH 并去掉 `.exe` 后缀使用。

### 验证安装

```bash
ilink-hub --version
```

---

## Step 3：启动 Hub（本机运行场景）

> 连远程已有 Hub 的用户跳过本步，直接用远程 Hub URL。

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
# 简单后台运行：
nohup ilink-hub serve --addr 0.0.0.0:8765 > ~/.ilink-hub/hub.log 2>&1 &
echo $! > ~/.ilink-hub/hub.pid
```

服务器部署（systemd）见 `docs/deployment/linux-systemd.md`。

---

## Step 4：注册后端客户端

Hub 侧注册一个后端，拿到虚拟 Token 供客户端使用：

```bash
ilink-hub register --hub-url http://127.0.0.1:8765 --name my-ai --label "My AI"
# 输出：
#   WEIXIN_BASE_URL=http://127.0.0.1:8765
#   WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxx
```

也可通过 Web 管理面板 `http://127.0.0.1:8765/hub/ui` 注册与管理。

> 若 Hub 设置了 `ILINK_ADMIN_TOKEN`，注册需带 `Authorization: Bearer <admin token>`。

---

## Step 5：接本机 CLI（bridge，可选，独立项目）

如需「微信消息 → 本机命令（Claude Code / Cursor / Codex 等）」，前往独立项目
[`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)：

1. 安装 `im-agentproc`（原 `ilink-hub-bridge`）。
2. 创建 profile YAML（`executor: claude-code` 等）。
3. 启动 bridge，`WEIXIN_BASE_URL` 指向 Hub，自动注册拿到 `vhub_…`。
4. 微信里 `/use <注册名>` 切到该后端。

具体命令与字段以 im-agentproc 仓库文档为准。

---

## Step 6：微信中切换后端

在微信中发送 Hub 命令（与已绑定的微信机器人对话）：

```
/list
```

Hub 回复所有已注册客户端，切换到目标后端：

```
/use my-ai
```

此后发出的普通消息都会路由给该后端处理。

---

## 验证自测清单

完成以上步骤后逐项确认：

- [ ] `ilink-hub --version` 有输出
- [ ] （本机运行）`curl http://127.0.0.1:8765/health` 返回 `{"status":"ok"}`
- [ ] 微信 `/list` 显示已注册客户端
- [ ] 微信发一条普通文字，目标后端被触发并回复

---

## 常见问题排查

**Hub 无法启动 / 端口占用**
```bash
lsof -i :8765   # 查看占用端口的进程
```

**注册失败（401）**
- Hub 开了 `ILINK_ADMIN_TOKEN`，注册时需带相同值
```bash
export ILINK_ADMIN_TOKEN=与 Hub 配置一致的值
```

**Hub 不可达**
```bash
curl <HUB_URL>/health   # 测试 Hub 是否可达
```

---

## 下一步

- 接本机 CLI（Claude Code 等）→ [im-agentproc](https://github.com/jeffkit/im-agentproc)
- 部署到服务器 → 参考 `docs/deployment/` 目录
- 微信命令参考 → `docs/reference/commands.md`
