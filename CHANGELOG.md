# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### agentproc 协议对齐 Step 1：Rust bridge 协议改名（2026-06-26）

**⚠️ Breaking Change** — bridge ↔ profile 进程间的 P0 协议变量名从 `ILINK_*` 改名为 agentproc v0.3.0 的 `AGENT_*`。用户**自定义 YAML** 中若硬编码了 `cli_session_first_line_prefix: "ILINK_SESSION:"` 需更新为 `"AGENT_SESSION:"`。自定义 profile 脚本若读取 `ILINK_MESSAGE`/`ILINK_SESSION_ID`/`ILINK_PARTIAL:` 等需改读 `AGENT_*`。Hub 自身配置变量（`ILINK_ADMIN_TOKEN`、`ILINK_HUB_MASTER_KEY`、`ILINK_CORS_ORIGINS`、`ILINK_TOKEN`、`ILINK_BASE_URL` 等）**不变**。

**协议名映射**

| 旧 | 新 |
|----|----|
| `ILINK_MESSAGE` | `AGENT_MESSAGE` |
| `ILINK_SESSION_ID` | `AGENT_SESSION_ID` |
| `ILINK_SESSION_NAME` | `AGENT_SESSION_NAME` |
| `ILINK_FROM_USER` | `AGENT_FROM_USER` |
| `ILINK_STREAMING` | `AGENT_STREAMING` |
| `ILINK_PARTIAL:` | `AGENT_PARTIAL:` |
| `ILINK_SESSION:` | `AGENT_SESSION:` |

**新增契约**

- 注入 `AGENT_PROTOCOL_VERSION` 环境变量（值为常量 `AGENTPROC_PROTOCOL_VERSION = "0.3"`），供 profile feature-detect。
- 解析 `AGENT_ERROR:<json-encoded-string>` 行：解码后通过现有 partial 通道转发给用户（最小实现；完整错误回复通道留待后续 step）。

**保留原名（ilink-hub 自有机制，不在 agentproc 协议范畴）**

- `ILINK_CONTEXT_TOKEN`（Hub 内部 context token）
- `ILINK_ITEM_TYPE` / `ILINK_IMAGE_URL` / `ILINK_FILE_URL` / `ILINK_FILE_NAME` / `ILINK_VIDEO_URL`（附件契约；agentproc `AGENT_ATTACHMENTS` 仍为 draft，附件对齐留待后续 step）

**后续步骤**（非本次）

- Step 2：删除 `sdk/python`、`sdk/node`，改用 agentproc 官方包；`examples/` import 替换
- Step 3：`docs/knowledge/bridges/profile-protocol.md` 等文档同步为"引用 agentproc spec"
- Step 4：内置 Rust builtin profile 归属决策

### 架构评审修复（2026-06-21）

**修复**

- **executor.rs OOM 截断**：`MAX_CLI_CAPTURE_BYTES` 触发后真正硬截断已读 buffer（之前只 warn，line 仍被 read_line 读入丢弃但未切断流；现在超限后该 line 也被截断到剩余配额，所有后续 line 跳过），避免恶意/异常 CLI 持续输出大流。
- **vtoken schema 校验（SEC-003 增强）**：`extract_vtoken` 在 `Bearer` 解析后加 `^vhub_[a-f0-9]{32}$` schema 过滤。恶意/错误配置客户端注入 iLink 风格 token（`botid@im.bot:secret`）会被立即拒绝。
- **清理 zombie metric `ilink_hub_dispatcher_lagged_total`**：该指标自 broadcast→mpsc 迁移后永远为 0，保留会污染 dashboard 速率计算。已在 0.2.5 之前发布周期内移除；任何基于该指标的告警需迁移至 `ilink_hub_dispatch_latency_ms` histogram（覆盖完整 dispatch 延迟）。
- **`apply_placeholders` shell 注入加固**：`{{MESSAGE}}` / `{{SESSION_ID}}` / `{{SESSION_NAME}}` 在注入到 argv / cwd / env 之前会拒绝 NUL / `\n` / `\r` 字节。微信用户消息含换行/控制字符时直接失败并返回清晰错误，避免 `bash -c "$1"` 类包装的 argv breakout。
- **main.rs Ctrl+C / SIGTERM 处理不再 panic**：`shutdown_on_signal` 改 `Result` 传递，handler 安装失败时记 warn 而非直接 abort。
- **`unsafe libc::kill` 加 SAFETY 注释**：明确 PID 来自自管的 `tokio::process::Child`、`ESRCH` 是期望路径、kernel 不跨进程 reuse 短窗口。

**性能**

- **Broadcast 路径 `Arc<WeixinMessage>` 共享**：`MessageQueue` 新增 `push_shared(vtoken, Arc<base>, context_token, hub_ext)`，broadcast 路径 N 个在线后端时只对 base 做一次 Arc clone（N 个 vtoken 各自只 clone 共享的 `item_list` Arc 引用 + cheap owned `vctx` string），不再做 N 次完整 `WeixinMessage::clone`。新增单元测试 `test_push_shared_does_not_clone_heavy_payload` 用 `Arc::strong_count` 守住实现。
- **`qr_last_ready` 改同步 `std::sync::Mutex`**：值是 `Option<Event>`，SSE handler 取出后立即 clone 出 owned，不再需要 `tokio::Mutex::lock().await` 的调度开销。

**可观测性 / 安全文档**

- **`docs/deployment/security.md` 新增「危险配置：绝对禁止的组合」章节**：列出 `ILINK_ADMIN_INSECURE_NO_AUTH=1` + 公网监听、未设 `ILINK_ADMIN_TOKEN`、Hub 直连公网、bash -c wrapper 等生产灾难组合，并提供 4 步 staging 验证清单（含本次新增 vtoken schema 校验和 placeholder 加固的回归测试点）。
- **`HubState` 锁顺序约定文档**：在 `src/hub/mod.rs` 顶部 doc 写明 `router → quote_index → registry` 严格顺序，并新增 `with_router_and_registry` facade 方法让多锁组合只在受控位置出现。

**测试**

- **新增 3 个 queue 集成测试**：`test_broadcast_path_full_queue_drops_oldest_not_newest`、`test_push_shared_does_not_clone_heavy_payload`（`Arc::strong_count` 守住实现）、`test_concurrent_pushes_preserve_message_count`（8 × 50 并发 push 不丢消息）。
- **新增 5 个 vtoken schema 校验单测**：`is_valid_vtoken` 的接受/拒绝边界（32 hex / ilink 风格 / 大小写 / 长度 / 非法字符）。
- **新增 4 个 placeholder 加固单测**：NUL / LF / CR 注入路径全部返回 `PlaceholderError::UnsafeValue`。

**Defer / 下次 PR**

- 拆分 `bridge/manager.rs` (1367 行) → `manager/{types,discovery,process,signal}.rs` — 工作量大，需要独立 worktree + PR review
- 拆分 `bridge/builtin/claude_code.rs` (1141 行) — 涉及 SDK 协议 + 流式解析
- 拆分 `server/routes.rs` (1302 行) → `routes/{ilink,hub,admin,metrics}.rs`
- 拆分 `store/store_tests.rs` (2140 行) → 各模块 `_tests.rs`
- 错误类型统一（HubError vs anyhow）

### Bridge — claude_code 多模态（图片 + PDF/文本文件）支持

### Bridge — claude_code 多模态（图片 + PDF/文本文件）支持

**新增**

- **claude_code 内置 bridge 支持图片输入**：当 bridge 检测到 `ILINK_IMAGE_URL` 环境变量（由前面几层的 media env 注入产生），自动切换到 Claude Code CLI 的 `--input-format stream-json --output-format stream-json` 双向流模式，在 stdin 上写入一行 `SDKUserMessage`（与 TS SDK 内部协议一致），`content` 字段为 `[text block, image block]` 数组，image block 携带 base64 编码的图片。图片在发送前通过 reqwest 从微信 CDN 下载，遵循 Anthropic API 5MB 限制。Session 续接（`--resume`）通过 `SDKUserMessage.session_id` 字段保留。流式 output 解析（`ILINK_PARTIAL`、`ILINK_SESSION`）保持不变。
- **claude_code 内置 bridge 支持 PDF / 纯文本文件输入**：当 bridge 检测到 `ILINK_FILE_URL` 环境变量时，把下载后的文件以 Anthropic `document` content block 写入 `SDKUserMessage.content`（`type: "document"`，`source.type: "base64"`，`media_type: application/pdf` 或 `text/plain`）。下载时强制 32MB 上限（Anthropic 文档块硬限制），并对非 PDF / 纯文本的 media type 提前拒绝（视频、zip、exe 等会立即在 bridge 日志中给出明确错误，不浪费一次 CLI 调用）。
- **图片与文件可同时下发**：当 `ILINK_IMAGE_URL` 与 `ILINK_FILE_URL` 都存在时，`content` 数组依次为 `[text, image, document]`，对应 Anthropic 协议里多模态拼接的合法形态。

**已知限制**

- **视频不支持**：Anthropic Messages API 没有 `video` content block，整个 ilink-hub 在用户→AI 这一步都不可能支持视频输入。微信收到的视频会落到「无法路由」日志里，bridge 不会尝试下载或转发。
- **非 PDF / 非纯文本文件不支持**：`document` 块只接受 `application/pdf` 和 `text/plain`。其他类型（CSV / Excel / Word / 压缩包 / 可执行文件等）需要走 Anthropic Files API 流程，超出 stream-json 协议范围。
- **CLI 实测通过（2026-06-18）**：本地 Claude Code CLI `2.1.177` + 模型 `MiniMax-M3` 上用 `--input-format stream-json --output-format stream-json --verbose --model MiniMax-M3` 跑通：128×128 PNG image block 正确识别图片为 grayscale，612 字节测试 PDF document block 正确读出文本内容，session 续接 / 流式 partial 输出 / `ILINK_SESSION` 行均按预期工作。注：极小（67 字节 1×1 透明 PNG）的图会被 API 路径拒绝（`400 invalid params (2013)`），属于 API 侧对图片最小尺寸的隐含要求，不是协议问题——实际微信图片通常远大于此。

**参考**：协议细节见 `fake-cc` 项目 `src/server/directConnectManager.ts:130` 和 `src/utils/teleport/api.ts:376`。

## [0.2.0] — 2026-06-17

### Bridge — B-01 session worker 指数退避

**修复**

- **CLI 崩溃后增加指数退避**：`run_session_worker` 对连续失败的 `handle_one_message` 调用实施指数退避（1s → 2s → 4s → … 最大 60s），防止 CLI 不可用时产生 tight crash-loop。成功处理后退避计数器重置为 0。

### Pairing — SEC-002 Scanned 阶段 60s 窗口

**修复**

- **QR 扫码后 confirm 窗口收窄至 60s**：`PairingSession` 新增 `scanned_at` 字段，`mark_scanned` 记录扫码时刻，`is_expired`/`should_evict` 对 `Scanned` 状态使用 `SCANNED_TTL = 60s` 而非 `PAIRING_TTL = 600s`，降低 replay 窗口（SEC-002）。新增测试 `scanned_session_expires_after_scanned_ttl_not_pairing_ttl`。

### Server — sendmessage DB 查询超时保护

**修复**

- **`sendmessage` 中 `get_active_session_name` / `set_backend_session` 添加 5s 超时**：SQLite 在高并发写入时若锁竞争严重，这两个调用原本无超时保护，现在与现有 `resolve_context_token_full` 调用保持一致的 5s 超时，超时时打印 `WARN` 并降级处理（不阻断消息发送）。

### Relay — SEC-011 URL 解码路径穿越防护 + TO-03 WebSocket 单帧超时

**修复**

- **`is_allowed_relay_path` 增加 URL 解码检查**：新增 `percent-encoding` 依赖，对 relay 转发路径在字面和解码两个层面均检查 `..`，防止 `%2e%2e` / `%2E.` / `.%2e` 等编码绕过（SEC-011）。新增 5 个对抗性测试用例。
- **WebSocket 单帧 120s 空闲超时**：`run_relay_session` 中 `read.next()` 现在受 `WS_IDLE_TIMEOUT_SECS = 120` 保护，超时后打印 `WARN` 并触发重连，防止半开连接无限挂起（TO-03）。

### Hub — 优雅停机队列 drain（ADR-001 方案 A）

**新增**

- **优雅停机时等待消息队列清空**：SIGTERM 触发 graceful shutdown 后，Hub 不再立即关闭 HTTP 连接，而是等待所有 vtoken 的 `InMemoryQueue` 被 bridge poll 到清空为止，最多等待 `ILINK_SHUTDOWN_DRAIN_SECS`（默认 30 秒）。超时后打印 `WARN` 日志，报告未投递消息数，然后继续关闭。设置 `ILINK_SHUTDOWN_DRAIN_SECS=0` 可禁用该等待。此改动消除了计划性重启（升级）场景下的消息丢失。

**文档**

- `docs/adr/001-message-queue-persistence.md`：记录三种持久化方案（优雅 drain / DB WAL / 停机快照）的决策过程与实施细节。
- `docs/adr/002-in-memory-state-inventory.md`：内存状态全量盘点，记录各组件重启后的影响范围。
- `docs/adr/003-sqlite-single-connection.md`：SQLite 单连接 `max_connections(1)` 设计决策与升级路径。
- `docs/adr/004-fire-and-forget-persist.md`：ContextToken fire-and-forget 持久化的权衡与可观测性说明。

### Store — DB 迁移版本追踪（H-1 修复）

**修复**

- **`run_migrations` 增加 `schema_version` 版本追踪**：v1–v5 各步骤在执行前先检查 `schema_version` 表，已应用的版本跳过，未应用的版本顺序执行，彻底解决"双轨维护"问题（架构审计 H-1）。每次 Hub 启动时迁移幂等，重复运行不会重跑已完成步骤。
- **ALTER TABLE 失败不再静默吞掉**：v3（`CREATE UNIQUE INDEX`）和 v4（`CREATE INDEX`）失败时返回错误并阻断启动，而非 `warn!` 继续。v4 `ALTER TABLE ADD COLUMN` 正确处理"列已存在"的幂等场景（兼容从无 `schema_version` 版本升级的数据库）。
- **修复 SQLite `ALTER TABLE ADD COLUMN` 兼容性**：SQLite 禁止将 `CURRENT_TIMESTAMP` 作为 `ALTER TABLE ADD COLUMN` 的 DEFAULT（被视为非常量表达式）。改为添加可空列 `TEXT`，所有 INSERT 语句显式传入 `CURRENT_TIMESTAMP`；`list_recent_context_tokens` 使用 `COALESCE(created_at, '')` 处理历史 NULL 行。
- **`record_migration_run` 改为幂等**：使用 `ON CONFLICT DO NOTHING`，防止重复插入触发主键冲突。

**新增测试**（`store::store_tests` 模块）：
- `test_schema_version_tracking`：验证新建 DB 所有 5 个迁移均已应用
- `test_migration_idempotency`：验证多次调用 `run_migrations` 不报错且版本不变
- `test_migration_incremental_from_v2`：模拟 v2 旧库，验证 v3-v5 可增量升级

**同步 `migrations/` SQL 文件**：
- `migrations/0000_schema_version.sql`：`schema_version` 表文档参考（注：由代码自动创建，此文件为文档用途）
- `migrations/0005_messages.sql`：新增，补全 v5 消息表迁移文件
- `migrations/0001–0004`：`datetime('now')` 统一改为 `CURRENT_TIMESTAMP`

## [0.1.22] — 2026-06-17

### Bridge — Cursor / Claude Code 修复

**修复**

- **Cursor bridge tool-use 场景回复丢失**：当 Cursor agent 在两轮之间使用工具时，最终回复文本只出现在 `result.result` 字段，而没有对应的 `assistant` 事件。原来的代码只从 `assistant` 事件流式发送 `ILINK_PARTIAL`，导致该场景下用户收不到回复。现在在 `result` 事件时判断 `result.result` 是否与最后一次发送的 partial 相同，若不同则额外发送一次 `ILINK_PARTIAL`。
- **claude_code.rs 真实 JSON 数组格式回归测试**：新增基于 CLI v2.1.153+ 真实输出（含 `rate_limit_event`、`system/init` 等额外字段）的解析回归测试，防止序列化结构变更导致 oneshot 模式静默失效。

**调整**

- **Bridge 超时从 600s 调整为 1800s**：`cursor-agent.example.yaml` 和 `profiles-builtin.yaml` 中 `timeout_secs` 由 600 调整为 1800，避免复杂任务因超时被截断。

## [0.1.21] — 2026-06-16

### Hub — 新增 `@<后端>` 快捷指令

**新增**

- **`@<名称> <消息>` 临时 @ 后端**：无需 `/use` 切换，直接 `@` 一个后端并发送消息，即可**临时**在该后端上**新建一个会话**处理这条消息，不改变当前 `/use` 的后端与活跃会话（性质与「引用回复」类似）。后端名取第一个空格之前的部分，名称与 `/use <名称>` 一致；`@` 优先级高于引用回复与当前路由；未匹配到已注册后端时整条消息按普通文本正常路由。要继续该临时会话，引用其回复即可。`/help` 帮助文案与 `docs/reference/commands.md` 已同步说明。

### Bridge — ILINK_PARTIAL 流式支持

**新增**

- **内置 cursor profile 类型**：新增 `type: cursor`，自动管理 `--resume` 续接上下文，并通过 `ILINK_PARTIAL` 实现流式输出。
- **内置 codex / agy profile 类型**：新增 `type: codex` 和 `type: agy`，统一升级为 `ILINK_PARTIAL` 流式。
- **全部 CLI profile 升级流式**：`upgrade all CLI profiles to ILINK_PARTIAL streaming`。
- **Bridge 本机主机名自动注册标签**：`fix(bridge): use local hostname as auto-registration label`。

**修复**

- **Hub 过滤空的 session-persist-only 消息**：footer 追加前先过滤空消息，避免发出空白气泡。
- **Relay 客户端会话保活**：修复 relay client 每 5 秒被取消的问题，改为保持长连接。
- **Hub 快捷指令与引用回复路由 fallback**：新增命令快捷方式，引用回复路由 footer-based fallback。
- **Hub 出站 footer 格式**：em dash 前缀改为 markdown hr。

### 安全变更

- **默认监听地址**：`serve` 默认监听地址由 `0.0.0.0:8765` 调整为 `127.0.0.1:8765`，防止默认情况下对局域网暴露未授权的管理接口。如果需要外部暴露（例如在 Docker 容器、虚拟机或需要局域网访问），请显式传入 `--addr 0.0.0.0:8765`。

### 桌面版

- **Desktop port 切换、bridge 目录隔离**：修复端口切换、bridge 目录隔离，以及 Hub URL 变更后自动重连。

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
