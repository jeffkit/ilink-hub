# ilink-hub-bridge：本地 CLI 后端

`ilink-hub-bridge` 是一个**独立进程**：对每条**用户文本消息**按 YAML 配置执行本机命令，把 **stdout** 作为回复发回微信。与 Recursive / OpenClaw 一样，通过 Hub 暴露的 iLink 兼容 API（`getupdates` / `sendmessage`）通信。

**连接 Hub 的两种方式（二选一）**

1. **扫码配对（推荐）**：不传 `--token` / `WEIXIN_TOKEN` 时，终端展示 Hub 客户端配对二维码，手机浏览器确认并填写客户端名称后，虚拟 Token 写入 **`~/.ilink-hub/bridge-credentials.json`**。再次启动会自动读入；需要换机或换 Hub 时加 **`--pair`** 重新扫码。路径可用 **`ILINKHUB_BRIDGE_CREDS`** 或 **`--cred-file`** 覆盖。  
2. **显式 Token**：使用 `ilink-hub register` 得到的 `vhub_…`，通过 `--token` 或环境变量 **`WEIXIN_TOKEN`** 传入。

Hub **不执行**你的 CLI，仍只做 iLink 代理；命令执行只发生在运行 bridge 的机器上。

::: tip 想先跑通再读细节？
直接跟做 **[5 分钟上手（echo 链路）](./quick-try.md)**，再回到本页查字段与进阶用法。
:::

## 适用场景

| 场景 | 说明 |
|------|------|
| 快速验证 Hub / Token / 路由 | 用 `echo` 或脚本确认消息能到本机 |
| 接 Claude Code、Codex、Gemini CLI 等 | 把 `command` / `args` 换成官方 CLI，用占位符塞入用户问题 |
| 与 Recursive / OpenClaw 并存 | 多注册一个 `--name`，用 `/use` 切换活跃后端 |

## 架构关系

```mermaid
flowchart LR
  WX[微信用户]
  ILINK[微信 iLink]
  HUB[iLink Hub]
  B[ilink-hub-bridge]
  CLI[本机进程]

  WX <--> ILINK
  ILINK <--> HUB
  HUB <-->|getupdates / sendmessage| B
  B --> CLI
```

与 [什么是 iLink Hub？](/guide/what-is-ilink-hub) 中的多后端模型一致：bridge 只是**又一个**虚拟 Token 客户端。

## 获取程序

| 方式 | 说明 |
|------|------|
| **Homebrew（macOS）** | `brew install ilink-hub` 同时安装 `ilink-hub` 与 `ilink-hub-bridge`（见 [安装](/guide/installation)） |
| **源码构建** | 仓库根目录 `cargo build --release --bin ilink-hub-bridge` |
| **cargo install** | `cargo install ilink-hub` |
| **Release 预编译** | [Releases](https://github.com/jeffkit/ilink-hub/releases) 中的 `ilink-hub-bridge-*` 资产 |

## 前置条件

1. Hub 已运行并完成微信侧绑定（见 [快速开始](/guide/getting-started)）。
2. **任选其一**：用 [扫码配对](./quick-try.md) 在手机上为该 bridge 起名并完成注册；或事先执行 `ilink-hub register` 取得 `WEIXIN_TOKEN`。
3. 若 Hub 上已有多个客户端，在微信中 [切换路由](/reference/commands)：`/use <配对时填写的名称>`。
4. 运行 bridge 的机器能访问 Hub 的 HTTP 端口。

## 最小启动示例

若已准备好 `ilink-hub-bridge.yaml`（可先使用 [echo 示例](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/echo.example.yaml)）：

**方式 A：扫码配对（不传 token）**

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

首次按终端提示扫码；成功后凭证保存在 `~/.ilink-hub/bridge-credentials.json`。

**方式 B：显式虚拟 Token**

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
export WEIXIN_TOKEN=vhub_xxxxxxxx
ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

### 命令行参数

| 参数 | 环境变量 | 说明 |
|------|-----------|------|
| `--hub-url` | `WEIXIN_BASE_URL` | Hub 根 URL（无路径后缀） |
| `--token` | `WEIXIN_TOKEN` | 可选；省略则读配对凭证文件或发起扫码 |
| `--cred-file` | `ILINKHUB_BRIDGE_CREDS` | 配对凭证 JSON 路径，默认 `~/.ilink-hub/bridge-credentials.json` |
| `--pair` | — | 忽略已存凭证，强制重新扫码配对 |
| `--config` | — | YAML 路径，默认 `./ilink-hub-bridge.yaml` |

## 配置字段（YAML）

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `command` | string | （必填） | 可执行文件名或绝对路径 |
| `args` | string 数组 | `[]` | 参数；支持占位符（见下） |
| `stdin` | `none` / `message` | `none` | `message` 时将用户消息全文以 UTF-8 写入子进程 stdin |
| `cwd` | string | 不设置 | 子进程工作目录（适合固定到某项目目录再跑 CLI） |
| `env` | map | `{}` | 额外环境变量（值支持占位符） |
| `timeout_secs` | number | `300` | 单条消息等待子进程的最长时间（秒） |
| `max_reply_chars` | number | `8000` | 回复按 **Unicode 字符数** 截断上限 |
| `truncation_suffix` | string | `…(输出已截断)` | 超长时在末尾追加的提示 |
| `skip_bot_messages` | bool | `true` | 忽略 `message_type == 2`（机器人侧消息），避免回路 |
| `require_text` | bool | `true` | 无文本时是否仍触发 CLI；`true` 则忽略纯图片/语音等 |
| `send_error_reply` | bool | `true` | CLI 非零退出或超时时，是否向用户发简短错误说明 |
| `include_stderr_in_reply` | bool | `false` | 成功时是否把 stderr 拼在 stdout 后面一并发出 |

### 占位符

在 `args` 与各 `env` **值**中可使用：

| 占位符 | 含义 |
|--------|------|
| `{{MESSAGE}}` | 当前用户消息的文本 |
| `{{FROM_USER_ID}}` | 上游 `from_user_id`（按需使用） |

`args` 以 **JSON/YAML 数组** 传给进程，**不经过 shell**，可避免常见注入；请勿自行拼 `sh -c` 再把用户原文塞进去。

::: warning 安全
Bridge 与 Hub 管理员权限无关：任何能向该微信会话发消息的人，都可能触发你配置的命令。请控制 Hub 暴露范围，并阅读 [安全建议](/deployment/security)。
:::

## 与桌面版（Tauri）的关系

当前 [桌面路线图](/desktop-tauri-roadmap) 中的壳主要嵌入 Hub；后续可在应用内一键拉起 bridge 子进程并写入配置，无需修改 iLink 协议。

## 更多示例

仓库内维护的示例（复制后按本机修改）：

- [echo.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/echo.example.yaml) — 调试链路  
- [claude-code.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/claude-code.example.yaml) — Claude Code CLI 模板  
- [codex.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/codex.example.yaml) — Codex CLI 模板  

官方子命令以各工具 `--help` 为准，模板中的 flag 可能随版本变化。

## 与「配置 AI 客户端」文档的关系

Bridge **不是** Recursive 插件，而是独立二进制；配置方式见 [配置 AI 客户端 — 与 wechatbot Echo 并列说明](/guide/client-config)。

## 常见问题

见 [FAQ](/guide/faq#bridge-no-msg) 中与 bridge 相关的条目。

---

最后更新：2026-06-07
