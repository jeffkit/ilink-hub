# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Hub — 新增 `@<后端>` 快捷指令

**新增**

- **`@<名称> <消息>` 临时 @ 后端**：无需 `/use` 切换，直接 `@` 一个后端并发送消息，即可**临时**在该后端上**新建一个会话**处理这条消息，不改变当前 `/use` 的后端与活跃会话（性质与「引用回复」类似）。后端名取第一个空格之前的部分，名称与 `/use <名称>` 一致；`@` 优先级高于引用回复与当前路由；未匹配到已注册后端时整条消息按普通文本正常路由。要继续该临时会话，引用其回复即可。`/help` 帮助文案与 `docs/reference/commands.md` 已同步说明。

### 安全变更

- **默认监听地址**：`serve` 默认监听地址由 `0.0.0.0:8765` 调整为 `127.0.0.1:8765`，防止默认情况下对局域网暴露未授权的管理接口。如果需要外部暴露（例如在 Docker 容器、虚拟机或需要局域网访问），请显式传入 `--addr 0.0.0.0:8765`。

## [0.1.20] — 2026-06-09

### Login — QR 登录稳定性修复

**修复**

- **QR 登录超时过短**：`get_qrcode_status` 是长轮询接口，服务端会持有连接约 30 秒后返回。原 `reqwest` 客户端超时恰好也是 30 秒，导致请求被客户端提前断开，轮询循环报错退出，服务器端（大陆外）尤为明显。现将客户端超时从 30 秒提高至 120 秒。
- **网络错误未重试**：`send().await` 出现网络层错误时会将错误向上传播，终止整个 QR 登录流程。现改为捕获错误、打印 `warn!` 并在 2 秒后重试，与解析错误处理一致。
- **轮询次数与时间窗口调整**：`MAX_ATTEMPTS` 从 120 调整为 60，配合每次最长 120 秒的超时，总等待窗口约 30 分钟，足够用户扫码。

### 文档 — 服务器部署与 Bridge 远程连接

**新增**

- **Linux / VPS 部署（systemd）**：新增 `docs/deployment/linux-systemd.md`，覆盖从源码编译、创建 systemd 服务、首次微信登录（Token 复用 / 终端扫码 / 本地代扫）到版本更新全流程。
- **Bridge 连接远程 Hub**：新增 `docs/bridge/remote-hub.md`，覆盖公网直连、SSH 端口转发、macOS launchd 持久化（SSH 隧道 + Bridge Manager 双服务）及 Linux systemd 持久化方案，含 PATH 配置注意事项和排查命令。

## [0.1.18] — 2026-06-09

### Hub — 安全与稳定性修复

**修复**

- **死锁修复**：`register_client_in_hub` 与 `unregister_client_in_hub` 存在锁顺序反转（ABBA 死锁），并发 Admin 操作时 Hub 会静默挂死。统一锁获取顺序为 registry → router，消除双锁嵌套。
- **鉴权绕过修复**：`sendtyping`、`getuploadurl`、`getconfig` 未校验 vtoken 是否已注册，任意 Bearer token 均可绕过访问控制。现统一应用 registry 注册表校验。
- **`upsert_client` vtoken 冲突静默失效**：重启后新 vtoken 写不进数据库，内存与 DB 永久不一致。已修复 `ON CONFLICT` 子句，加入 `SET vtoken = EXCLUDED.vtoken`。
- **Broadcast 共享 vctx**：广播多后端时所有后端收到同一 `context_token`，只有最后一个回复生效。现为每个后端按 `conv_key@vtoken` 分配独立 vctx。
- **上下文缓存预热加载最旧记录**：`list_recent_context_tokens` 缺少 `ORDER BY`，重启后加载的是最旧 500 条而非最近活跃会话。已改为 `ORDER BY rowid DESC LIMIT 500`。
- **Health checker 不响应 shutdown**：后台协程未监听 shutdown channel。现改用 `tokio::select!` 监听 shutdown 信号，支持 Tauri 桌面版重启场景。
- **Admin token 每次请求读 env var**：改用 `OnceLock` 只初始化一次；未设置时启动时打印 `warn!` 告警。
- **TOCTOU 竞态修复**：`resolve_vctx_for_message` 三次加锁释放存在竞态窗口。现改为锁外做 DB 查询，单次加锁内完成 check + seed + map。
- **`QuoteRouteIndex` 热路径 O(N) 扫描**：`evict_expired()` 移入独立后台任务 `spawn_quote_index_evictor`，每 5 分钟执行一次。
- **`RateLimiter` buckets 无界增长**：bucket 总数超 10,000 时触发清理，防止公网 relay 长期运行内存泄漏。
- **删除 `validate_session` 死代码**：该方法已不在启动路径调用。

### 文档 — 面向非技术用户的全面改写

**改进**

- **首页分流**：主按钮明确区分「不懂代码下载桌面版」和「会用终端快速开始」。
- **iLink 前置条件说明**：首页和关键页面均加入「什么是 iLink、如何申请」的说明。
- **桌面版提升为安装方式一**：原为「方式五」，现提至首位。
- **CPU 类型判断指引**：安装和快速开始页加入 Apple Silicon vs Intel 的判断方法。
- **每步加失败处理**：快速开始每个步骤新增折叠的「失败了怎么办」提示块。
- **删除错误描述**：`register-client.md` 中「Hub 只存哈希值」的错误说明已删除。
- **FAQ 重新排序**：新增「基本问题」分类，高频问题提至最前，更新桌面版 GUI 答案。
- **侧边栏重组**：第一组改为「开始使用」，顶部导航加「下载桌面版」入口。

### Hub — 加速启动与停机

**修复**

- **启动加速**：DB/CLI 中格式合法的 token 不再在启动时调用 `getupdates` 探测，Hub 可在 1 秒内监听；会话有效性改由 upstream polling 负责。token 过期（`-14`）时在 polling 中自动触发二维码重登。
- **停机 `getupdates` 长轮询**：收到 Ctrl+C / shutdown 信号后立即返回空结果，不再阻塞最多 30 秒；Axum graceful shutdown 可在亚秒级完成。

### Hub — Admin UI 编辑 workspace

**新增**

- **Admin UI**：每个 Connected Workspace 卡片支持 **Edit**，可修改 `name` 与 `label`（在线/离线均可）。
- **`PATCH /hub/clients/{name}`**：更新客户端名称与标签；重名时返回 `409`。

## [0.1.17] — 2026-06-08

### Hub — 管理后台删除离线后端

**新增**

- **Admin UI**：离线后端卡片显示 **Delete** 按钮，可清理 `/list` 中的失效注册项。
- **`DELETE /hub/clients/{name}`**：仅允许删除离线客户端；同步清理内存路由、消息队列与数据库中的 routing 记录。

## [0.1.16] — 2026-06-08

### Bridge — 稳定自动注册名

**修复**

- 自动注册默认使用 **`local-<hostname>-<config-stem>`**（如 `local-MacBook-ilink-claude`），不再每次生成随机 `local-<uuid>`，避免 `/list` 堆积失效后端。
- 凭证 JSON 保存 `client_name`；token 失效重注册时复用同一名称。

## [0.1.15] — 2026-06-08

### Hub — 多轮对话 session 连续性

**修复**

- **同一微信用户多轮对话**：微信/iLink 每条消息可能携带新的 `context_token`；Hub 现在按 `peer_user_id`（群聊则按 `group_id`）复用稳定的虚拟 `vctx`，Claude `--resume` 等 backend session ID 可跨消息保留。
- **Hub 重启恢复**：冷启动时从数据库查找该用户已有的 backend session 并预热内存映射。
- **回复来源脚注**：默认仅在 **同时在线的后端 ≥ 2** 时追加 `— 工作区名` 行（不再因历史离线注册项误触发）。

## [0.1.14] — 2026-06-08

### Bridge — Claude Code 可靠性

**修复**

- **YAML `cwd` 支持 `~`**：profile 的 `cwd: ~/projects/foo` 现在会正确展开为用户主目录，避免 spawn 报 `No such file or directory`。
- **`type: claude-code` 自调用**：内置 profile 子进程使用 `current_exe()` 而非依赖 PATH 中的 `ilink-hub-bridge`。
- **Claude 非零 exit 仍解析回复**：当 `claude --output-format json` 因模型错误等返回 exit 1 但 stdout 含 JSON `result` 时，将结果文本转发到微信，而非只显示 `command exited with status 1`。
- **Bridge vtoken 校验与自动重注册**（v0.1.13 起）：Hub 拒绝无效 token 时 bridge 自动删凭证并重新 `/hub/register`。

**说明**

- Profile YAML 的 `env.ILINK_CLAUDE_MODEL` 会注入到 `claude-code` 子进程；用于覆盖 Claude Code 默认模型（例如不可用的第三方模型）。

## [0.1.11] — 2026-06-08

### Bridge — P0 Exec Protocol & Profile SDK

**新功能**

- **P0 协议**：bridge 现在自动将 `ILINK_MESSAGE`、`ILINK_SESSION_ID`、`ILINK_SESSION_NAME`、`ILINK_FROM_USER`、`ILINK_CONTEXT_TOKEN` 注入到每个 profile 进程的环境变量中。自定义脚本和 SDK 无需在 YAML `env:` 段手动映射这些变量。
- **`type: claude-code` 语法糖**：profile 中设置 `type: claude-code` 即可使用内置 Claude Code 处理器，无需配置 `command`、`args`、`cli_session_first_line_prefix`，也不再需要 `ilink-claude-bridge.sh` 包装脚本。
- **`ilink-hub-bridge profile <type>` 子命令**：内置 profile 以独立子命令形式发布，遵守 P0 exec 协议，可在命令行直接测试：`ILINK_MESSAGE="你好" ilink-hub-bridge profile claude-code`。
- **Node.js SDK**（`sdk/node/`）：`@ilink-hub/profile` — 一个 `createProfile(handler)` 调用即可创建跨平台 profile，含 `loadHistory` / `appendHistory` JSONL 对话历史工具。
- **Python SDK**（`sdk/python/`）：`ilink-bridge-profile` — `create_profile(handler)` 同等功能的 Python 版本。
- **[`docs/bridge/profile-spec.md`](docs/bridge/profile-spec.md)**：新增 Bridge Profile P0 协议规范文档，涵盖协议契约、实现方式对比、YAML 配置示例、状态持久化指南。

**变更**

- `run_cli()` 签名新增 `from_user` 和 `context_token` 参数（内部变更，不影响 YAML 配置）。
- 示例 YAML [`docs/bridge/examples/claude-code-session.profiles.yaml`](docs/bridge/examples/claude-code-session.profiles.yaml) 重写为 `type: claude-code` 风格。

### Hub — 多 Session 支持（v0.1.10 继续）

- `/session list / new / use / delete` 命令
- `backend_sessions` 数据库表与 `active_session_name` 字段

---

## [0.1.10] — 2026-06-07

- Hub 内建多 session 管理（`/session` 命令）
- `ilink_hub_ext` 扩展字段（封装 `session_id`、`session_name`、`cli_session_id`）
- `ilink-claude-bridge.sh` 包装脚本（claude code --resume 连续对话）
