# ilink-hub-bridge：本地 CLI 后端

`ilink-hub-bridge` 是一个**独立进程**：对每条**用户文本消息**按 YAML 配置执行本机命令，把 **stdout** 作为回复发回微信。与 Recursive / OpenClaw 一样，通过 Hub 暴露的 iLink 兼容 API（`getupdates` / `sendmessage`）通信。

**连接 Hub 的方式（任选）**

1. **零交互（默认，本机最省事）**：不传 `--token`、且**凭证路径尚不存在**时，进程会自行调用 Hub 已有的 **`POST /hub/register`**（与任何其他客户端相同），生成随机客户端名、拿到 `vhub_…` 后写入 **`~/.ilink-hub/bridge-credentials.json`**。终端会打印 `已向 Hub 自动注册客户端「…」`，按提示在微信发 `/use <名称>` 即可。若 Hub 配置了 **`ILINK_ADMIN_TOKEN`**，请在本机同一环境导出该变量，否则注册会 401。可用 **`--register-name` / `ILINKHUB_BRIDGE_REGISTER_NAME`** 固定注册名。  
   **凭证文件已存在但损坏或 token 为空**时：为避免静默覆盖扫码配对结果，默认**不会**再自动注册；请删文件、改用 **`WEIXIN_TOKEN` / `--pair`**，或显式加 **`--force-register`**（会先删该路径再自动注册）。  
2. **扫码配对**：加 **`--pair`**（或你希望用手机确认时），走 Hub 通用配对流程；凭证仍写入上述 JSON。  
3. **显式 Token**：自行 `ilink-hub register` 或拷贝 `vhub_…`，通过 **`--token` / `WEIXIN_TOKEN`** 传入。

Hub 侧**不区分**调用方是不是 bridge：只看到普通的「注册客户端」与「长轮询下游」。

**不会**在「连不上 Hub」时自动改走扫码或注册：自动 `POST /hub/register` 失败会直接报错（连接错误时会提示检查 URL / 远程 Hub / `WEIXIN_TOKEN` / 待 Hub 就绪后再用 `--pair`）。需要扫码时请**自行**加 `--pair`。

**与是否本机安装 `ilink-hub` 无关**：只要 `WEIXIN_BASE_URL` 指向任意可达的 Hub（同事机器、内网服务器、公网域名均可），只装 bridge 即可；本机不必安装或启动 `ilink-hub`。

Hub **不执行**你的 CLI，仍只做 iLink 代理；命令执行只发生在运行 bridge 的机器上。

::: tip 想先跑通再读细节？
先跟做 **[5 分钟上手（echo 链路）](./quick-try.md)**；要接 **Claude Code / Cursor / Codex** 等本地 CLI，请看 **[使用指引](./USAGE.md)**。字段说明仍以本页为准。
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
| **cargo install** | `cargo install ilink-hub`（与 Hub 同属 **crates.io 上的同一个包**；默认会安装包内声明的多个二进制，含 `ilink-hub-bridge`；若只要 bridge 可加 `--bin ilink-hub-bridge`） |
| **Release 预编译** | [Releases](https://github.com/jeffkit/ilink-hub/releases) 中的 `ilink-hub-bridge-*` 资产 |

## 前置条件

1. Hub 已运行并完成微信侧绑定（见 [快速开始](/guide/getting-started)）。
2. 本机第一次跑 bridge：可不扫码；进程会自动 `POST /hub/register` 并保存凭证（见上文）。若 Hub 开了管理 Token，请配置 **`ILINK_ADMIN_TOKEN`**。
3. 若 Hub 上已有多个客户端，在微信中 [切换路由](/reference/commands)：按启动时终端提示的 **`/use <名称>`**。
4. 运行 bridge 的机器能访问 Hub 的 HTTP 端口。

## 最小启动示例

若已准备好 `ilink-hub-bridge.yaml`（调试可用 [echo 示例](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/echo.example.yaml)；真实 CLI 见 [使用指引](./USAGE.md)）：

**方式 A：自动注册（不传 token，无凭证文件时）**

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
# 若 Hub 设置了 ILINK_ADMIN_TOKEN：
# export ILINK_ADMIN_TOKEN=……
ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

首次运行成功后，凭证保存在 `~/.ilink-hub/bridge-credentials.json`（可用 `ILINKHUB_BRIDGE_CREDS` 改路径）。再次启动会直接读文件。

**方式 B：扫码配对**

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
ilink-hub-bridge --pair --config ./ilink-hub-bridge.yaml
```

**方式 C：显式虚拟 Token**

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
export WEIXIN_TOKEN=vhub_xxxxxxxx
ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

### 命令行参数

| 参数 | 环境变量 | 说明 |
|------|-----------|------|
| `--hub-url` | `WEIXIN_BASE_URL` | Hub 根 URL（无路径后缀） |
| `--token` | `WEIXIN_TOKEN` | 可选；省略则尝试读本地凭证、自动注册或 `--pair` 扫码 |
| `--cred-file` | `ILINKHUB_BRIDGE_CREDS` | 凭证 JSON 路径，默认 `~/.ilink-hub/bridge-credentials.json` |
| `--pair` | — | 忽略已存凭证，强制走 Hub 扫码配对 |
| `--force-register` | — | 凭证文件存在但无效时：删除该文件后重新走自动 `/hub/register`（默认在这种情况下会报错而不覆盖） |
| `--register-name` | `ILINKHUB_BRIDGE_REGISTER_NAME` | 自动注册时使用的客户端名（可选；默认随机 `local-<uuid>`） |
| `--config` | — | YAML 路径，默认 `./ilink-hub-bridge.yaml` |
| （环境） | `ILINK_ADMIN_TOKEN` | Hub 若要求管理端鉴权注册，需与 Hub 相同，供自动注册请求携带 |
| （环境） | `ILINKHUB_BRIDGE_DUMP_MSG` | 设为 `1` / `true` / `yes` 时，每条入站消息在 **stderr** 打印完整 `WeixinMessage` JSON，并逐项打印 `item_list[*].extra`（用于查看 iLink 嵌在 item 里的扩展字段，如引用信息是否落在 `extra`） |

### 调试：查看入站消息的 `extra`（引用回复等）

Hub 下发给 bridge 的体与 [`WeixinMessage`](https://github.com/jeffkit/ilink-hub/blob/main/src/ilink/types.rs) 一致：`MessageItem` 里除 `type` / `text_item` 外的字段会 serde **flatten** 进 **`extra`**。

```bash
export ILINKHUB_BRIDGE_DUMP_MSG=1
ilink-hub-bridge --config ./ilink-hub-bridge.yaml
```

微信里发一条**引用机器人消息**的回复，看终端 stderr：  
- 若引用元数据在 **`item_list` 某元素的 `extra`** 里，会单独打印出来。  
- 若上游把引用放在 **消息顶层**、而 `WeixinMessage` 没有对应字段，则在进 Hub 反序列化时已被丢弃，**这里也看不到**（需要抓 Hub 收到上游后的原始 JSON 才能确认）。

## 配置：单 Profile 与多 Profile

### 单 Profile（默认，与旧版兼容）

根级一个 `command`（必填）及下表字段即可；**不要**同时写顶层 `profiles`，否则会被识别为多 Profile 格式。

### 多 Profile（单进程、按前缀路由）

顶层包含 **`profiles`** 与 **`routing`**。每个 profile 拥有一套与单文件相同的执行字段（`command`、`args`、`timeout_secs` 等）；根级可写 `skip_bot_messages` / `require_text` / `send_error_reply`，对**所有** profile 生效。

| 字段 | 说明 |
|------|------|
| `profiles` | map：profile 名 → 该 profile 的执行配置（`command` 必填等，字段同下表） |
| `routing.default_profile` | 未命中任何前缀时使用的 profile 名 |
| `routing.strategy` | `fixed`：始终用 `default_profile`；`prefix`：按 `prefix_rules` 匹配（**先匹配先生效**，较长前缀请写在列表前面） |
| `routing.prefix_rules` | 仅 `strategy: prefix` 时需要非空；每项 `prefix` + `profile`；命中后 **去掉前缀** 的余文作为 `{{MESSAGE}}` / stdin |

完整示例：[multi-profile.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/multi-profile.example.yaml)。

## 配置字段（单 Profile 根级，或 `profiles.<name>` 内）

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
| `{{MESSAGE}}` | 当前用户消息的文本（多 Profile 的 `prefix` 模式下为**去掉匹配前缀后**的余文） |
| `{{FROM_USER_ID}}` | 上游 `from_user_id`（按需使用） |

`args` 以 **JSON/YAML 数组** 传给进程，**不经过 shell**，可避免常见注入；请勿自行拼 `sh -c` 再把用户原文塞进去。

::: warning 安全
Bridge 与 Hub 管理员权限无关：任何能向该微信会话发消息的人，都可能触发你配置的命令。请控制 Hub 暴露范围，并阅读 [安全建议](/deployment/security)。
:::

## 与桌面版（Tauri）的关系

当前 [桌面路线图](/desktop-tauri-roadmap) 中的壳主要嵌入 Hub；后续可在应用内一键拉起 bridge 子进程并写入配置，无需修改 iLink 协议。

## 更多示例

仓库内维护的示例（复制后按本机修改 `cwd`、认证方式与各 CLI 的 flag）：

- [multi-profile.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/multi-profile.example.yaml) — **单进程多 Profile**（`prefix` 路由示例）  
- [claude-code.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/claude-code.example.yaml) — **Claude Code**（`claude -p`）  
- [cursor-agent.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/cursor-agent.example.yaml) — **Cursor Agent**（`agent -p`）  
- [codex.example.yaml](https://github.com/jeffkit/ilink-hub/blob/main/docs/bridge/examples/codex.example.yaml) — **OpenAI Codex**（`codex exec`）  

**串联说明**（多 CLI、多凭证路径、`/use` 切换）见 **[使用指引](./USAGE.md)**。各工具子命令以官方 `--help` 为准，模板中的 flag 可能随版本变化。

## 与「配置 AI 客户端」文档的关系

Bridge **不是** Recursive 插件，而是独立二进制；配置方式见 [配置 AI 客户端 — 与 wechatbot Echo 并列说明](/guide/client-config)。

## 常见问题

见 [FAQ](/guide/faq#bridge-no-msg) 中与 bridge 相关的条目。

---

最后更新：2026-06-07
