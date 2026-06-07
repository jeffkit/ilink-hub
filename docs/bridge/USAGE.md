# ilink-hub-bridge 使用指引

本文说明如何从「零」到用 **ilink-hub-bridge** 把微信消息交给本机 **Code CLI**（Claude Code、Cursor Agent、Codex 等），以及如何管理多份配置与多客户端路由。

::: tip 路径选择
- **只想最快跑通**：先做 [5 分钟上手（echo）](./quick-try.md)。  
- **已会 echo，要接真实 CLI**：继续读本页；示例 YAML 在仓库 [`docs/bridge/examples/`](https://github.com/jeffkit/ilink-hub/tree/main/docs/bridge/examples)。  
- **字段字典**：见 [功能与配置](./README.md)。
:::

## 1. 环境准备

| 组件 | 说明 |
|------|------|
| **Hub** | 已启动并完成微信 iLink 绑定（见 [快速开始](/guide/getting-started)）。 |
| **bridge** | 与 Hub **同一套**安装方式即可带上：`brew install ilink-hub` 或 `cargo install ilink-hub` 或 [Release](https://github.com/jeffkit/ilink-hub/releases) 预编译包。 |
| **本机 CLI** | 已安装并在终端里能直接运行（如 `claude`、`agent`、`codex`），且具备非交互用法（见下文各节）。 |

Bridge **不要求**本机安装或运行 `ilink-hub` 二进制，只要 `WEIXIN_BASE_URL` 指向**可达**的 Hub。

## 2. 选一种连 Hub 的方式

1. **自动注册（默认）**：不传 token、且默认凭证文件**尚不存在**时，bridge 会调用 Hub 的 `POST /hub/register`，把 `vhub_…` 写入 `~/.ilink-hub/bridge-credentials.json`。若 Hub 开了 **`ILINK_ADMIN_TOKEN`**，请在同一 shell 里 `export` 相同值。  
2. **扫码**：`ilink-hub-bridge --pair …`（手机确认）。  
3. **显式 token**：`export WEIXIN_TOKEN=vhub_…` 或 `ilink-hub-bridge --token vhub_…`。  

若凭证文件**已存在但损坏或 token 为空**，默认**不会**静默覆盖（避免误伤扫码结果）；可删文件、`--pair` / `WEIXIN_TOKEN`，或 **`--force-register`**。详见 [功能与配置](./README.md)。

## 3. 最小运行命令

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
# 若 Hub 要求管理端鉴权注册：
# export ILINK_ADMIN_TOKEN=与 Hub 一致的值

ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

首次自动注册成功后，终端会提示在微信发 **`/use <客户端名>`** 把路由切到该 bridge（Hub 上若已有多个下游，这是必做的一步）。

## 4. 接本地 Code CLI：三份示例配置

下面三条链路都遵守同一规则：在 YAML 里用 **`{{MESSAGE}}`** 把用户发来的文字塞进 CLI 参数（或配合 `stdin: message`）。**不要把用户原文拼进 `sh -c`**，以免注入风险。

### 4.1 Claude Code（`claude`）

常见非交互用法为 **print 模式**（以本机 `claude --help` 为准）：

```yaml
command: claude
args: ["-p", "{{MESSAGE}}"]
stdin: none
cwd: /path/to/your/project
timeout_secs: 600
```

完整拷贝模板：[claude-code.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/claude-code.example.yaml)。  
认证：通常依赖本机已 `claude login` 或环境变量（如 `ANTHROPIC_API_KEY`），勿把密钥写进仓库。

### 4.2 Cursor Agent（`agent`）

Cursor 提供的 CLI 命令名为 **`agent`**（安装后应在 PATH 中；安装见 [Cursor CLI 文档](https://cursor.com/docs/cli/overview)）。非交互场景用 **`-p` / `--print`**：

```yaml
command: agent
args: ["-p", "{{MESSAGE}}"]
stdin: none
cwd: /path/to/your/project
timeout_secs: 600
```

完整模板：[cursor-agent.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/cursor-agent.example.yaml)。  
认证：脚本/自动化常用 **`CURSOR_API_KEY`**，或事先在本机执行 `agent login`。若希望 Agent 在无确认下改文件，需自行查阅官方文档是否使用 `--force` / `--yolo` 等 flag（**有安全风险**，仅在你信任的仓库目录使用）。

### 4.3 OpenAI Codex（`codex`）

非交互执行常见为 **`codex exec`**（以本机 `codex --help` 为准）：

```yaml
command: codex
args: ["exec", "{{MESSAGE}}"]
stdin: none
cwd: /path/to/your/project
timeout_secs: 600
```

模板：[codex.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/codex.example.yaml)。

### 4.4 调试专用：Echo

不涉及任何大模型，仅验证链路：[echo.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/echo.example.yaml)。

## 5. 多 CLI / 多项目：推荐做法

Hub **同一时间只把普通消息路由给一个「活跃」下游**。你可以：

**做法 A — 换配置文件（最常见）**  
准备多份 YAML（如 `bridge-claude.yaml`、`bridge-cursor.yaml`），每次只启动一个 bridge，按需改 `--config`。同一台机器若混用自动注册，建议为不同「逻辑后端」指定不同凭证路径，避免互相覆盖，例如：

```bash
export ILINKHUB_BRIDGE_CREDS="$HOME/.ilink-hub/bridge-claude.json"
ilink-hub-bridge --register-name my-claude --config ./bridge-claude.yaml
```

另开 Cursor 时再换 `ILINKHUB_BRIDGE_CREDS` 与 `--register-name my-cursor`。

**做法 B — 多进程 + `/use` 切换**  
为每个 CLI 各注册一个 Hub 客户端名，各跑一个 bridge 进程（各用不同 `WEIXIN_TOKEN` 或不同 `ILINKHUB_BRIDGE_CREDS`）。在微信里用 **`/use <名称>`** 切换当前对话走哪条链路。注意：未活跃进程仍会占用 Hub 连接，按需启停即可。

**做法 C — 一份 YAML 多 Profile（单进程）**  
在一份 YAML 里写 `profiles` + `routing`（`fixed` 或 `prefix`）。`strategy: prefix` 时按 `prefix_rules` 匹配，命中前缀会从 `{{MESSAGE}}` 中**剥掉**该前缀再交给对应 CLI。示例：[multi-profile.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/multi-profile.example.yaml)。仍与 Hub 上**一个**下游客户端、一条长轮询一致；与「多进程 + `/use`」可并存，按场景选用。

## 6. 自测清单

1. Hub `GET /health` 返回 `ok`。  
2. `ilink-hub-bridge --version` 与 Hub 版本符合预期。  
3. 微信发 **`/list`**：你的 bridge 客户端显示为在线。  
4. 发**非命令**普通文字，确认本机 CLI 被触发且微信收到 stdout 截断后的回复。  
5. 故意触发一次 CLI 失败，确认是否收到错误回执（由 YAML `send_error_reply` 控制）。

## 7. 发版后维护者：更新 Homebrew formula

`brew install ilink-hub` 使用仓库 **[jeffkit/homebrew-tap](https://github.com/jeffkit/homebrew-tap)** 中的 `Formula/ilink-hub.rb`，版本号与 **GitHub Release** 中 macOS 四个文件的 **sha256** 需与 tag 一致。

GitHub Actions 在推送 **`vX.Y.Z`** tag 并完成 Release 后，可在本仓库执行：

```bash
./scripts/homebrew-ilink-hub-shas.sh X.Y.Z
```

将输出的 sha256 填入 `homebrew-tap` 的 `ilink-hub.rb` 对应 `url` 行，并更新 `version "X.Y.Z"`，提交推送 tap 后用户即可 `brew update && brew upgrade ilink-hub`。

---

最后更新：2026-06-07
