# iLink Hub 架构审查报告

> 审查日期：2026-06-10
> 审查范围：全量代码（src/）+ Cargo.toml
> 状态：待处理

---

## 整体评估

项目是一个 Rust 编写的消息路由中枢，将真实微信 iLink Bot 账号的消息分发到多个 AI 后端（注册为虚拟 token 的客户端）。整体结构清晰，分层合理，有一定测试覆盖。**主要风险集中在：无界内存增长、锁争用热点、数据库迁移脆弱性、以及若干安全盲点。**

---

## 优先级汇总

| 编号 | 严重度 | 分类 | 简述 |
|------|--------|------|------|
| S-01 | **P0** | 安全 | 未设 `ILINK_ADMIN_TOKEN` 时管理接口完全开放 |
| A-04 | **P1** | 架构 | Store 迁移无版本管理，`rowid` 排序不可移植 |
| A-05 | **P1** | 可靠性 | 多锁加锁顺序仅靠注释，无编译时保证 |
| R-01 | **P1** | 可靠性 | `ContextTokenMap` / `SessionDispatcher.senders` 无界增长 |
| R-02 | **P1** | 可靠性 | `map_context_token` 存在 TOCTOU 竞态（先 SELECT 后 INSERT） |
| R-03 | **P1** | 可靠性 | 未知队列 backend 静默 fallback，配置错误不可见 |
| S-02 | **P1** | 安全 | 全局 `permissive CORS` 扩大管理接口攻击面 |
| S-03 | **P1** | 安全 | `{{MESSAGE}}` 直接注入 args，存在命令注入风险 |
| P-01 | **P1** | 性能 | `InMemoryQueue` 全局单锁，高并发时完全串行化 |
| 其余  | **P2** | 各类 | 代码重复、指标不完整、依赖落后等改进项 |

---

## 一、架构层面

### A-01 `HubState` 是单一全局神对象（God Object）

- **严重度**：P2 / 架构
- **位置**：`src/hub/mod.rs:67-116`
- **问题**：`HubState` 同时持有 `upstream`、`registry`、`pairing`、`queue`、`ctx_map`、`router`、`quote_index`、`store`、`metrics`、`shutdown`、`ilink_status`、`qr_tx`、`qr_last_ready`、`relogin_tx`，共 13 个字段。任何模块只需拿到 `Arc<HubState>` 即可访问全部内部状态，没有真正的访问边界。
- **建议**：将 `ilink_status + qr_tx + qr_last_ready + relogin_tx` 抽取为 `IlinkConnState`，将 `registry + router + quote_index` 抽取为 `RoutingState`，降低未来修改时的意外耦合。

### A-02 `dispatch_message` 函数过长（约 200 行），逻辑分支重复

- **严重度**：P2 / 可维护性
- **位置**：`src/hub/mod.rs:159-350`
- **问题**：`ForwardTo` 和 `Broadcast` 两个分支共享大量代码（`resolve_vctx_for_message`、`persist_context_token`、`build_hub_ext_for_vctx`），且 `Broadcast` 路径再次嵌套一层 per-vtoken 的相似逻辑，存在明显 DRY 违反。
- **建议**：提取 `dispatch_to_single_vtoken(state, msg, vtoken, session_override)` 辅助函数，`Broadcast` 路径改为遍历调用该函数。

### A-03 `handle_hub_command` 是 600+ 行的 match 超级函数

- **严重度**：P2 / 可维护性
- **位置**：`src/hub/mod.rs:352-693`
- **问题**：每个 `HubCommand` 变体的处理逻辑完全内联，高度重复的样板：获取 `vctx`、获取 `vtoken`、匹配 None 返回相同错误。
- **建议**：提取 `resolve_vctx_and_vtoken_for_user(state, msg, real_ctx, from_user_id) -> Option<(vctx, vtoken)>` 辅助函数，各 Session 子命令调用后仅实现核心差异逻辑。

### A-04 Store 手写 inline SQL 迁移，无版本管理

- **严重度**：P1 / 架构
- **位置**：`src/store/mod.rs:66-179`
- **问题**：迁移以 `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ... ADD COLUMN`（带 `let _ = ...` 忽略错误）的方式内联在 `migrate()` 函数中。废弃表 `backend_sessions`（v1）仍然每次启动时被创建；`rowid` 作为排序字段在 PostgreSQL 中**不存在**（MySQL 有但行为不同）；`ALTER TABLE ... ADD COLUMN NOT NULL DEFAULT ''` 在某些 MySQL 版本下会全表重建。
- **建议**：引入 `sqlx::migrate!("./migrations")` 宏，将每次 schema 变更放到带版本号的迁移文件中。将 `rowid` 排序改为 `created_at` 列并建索引。

### A-05 多锁加锁顺序仅靠注释维护，存在死锁风险

- **严重度**：P1 / 可靠性
- **位置**：`src/server/pairing.rs:93`（注释"Lock order: registry → router"），`src/hub/mod.rs` 多处
- **问题**：代码库中明确有注释要求 `registry → router` 加锁顺序，但没有任何编译时或运行时保证。若未来某处以 `router → registry` 顺序获取，就会产生死锁。
- **建议**：用 `parking_lot` 的 `lock_hierarchy` 特性，或封装一个 `HubLocks` 结构统一管理多锁获取顺序，或至少写一个 integration test 验证并发注册 + 路由场景不死锁。

---

## 二、可靠性

### R-01 `ContextTokenMap` / `SessionDispatcher.senders` 在内存中无界增长

- **严重度**：P1 / 可靠性
- **位置**：`src/hub/queue.rs:30-154`，`src/bridge/mod.rs:181`
- **问题**：`ContextTokenMap` 的四个 HashMap（`v_to_real`、`real_to_v`、`v_to_peer`、`conv_to_v`）只增不减。长期运行每个曾对话过的微信用户都会留下永久条目。`SessionDispatcher.senders` 中退出 worker 对应的 sender 条目，若对话 key 永远不再来消息，会永久留在 HashMap 中。
- **建议**：为 `ContextTokenMap` 增加 LRU 淘汰（按 `last_used` 时间）或定期 TTL 清理；为 `SessionDispatcher.senders` 增加空闲 channel 的主动清理机制（如 `tx.is_closed()` 扫描）。

### R-02 `map_context_token` 存在 TOCTOU 竞态

- **严重度**：P1 / 可靠性
- **位置**：`src/store/mod.rs:445-465`
- **问题**：先 `SELECT` 查是否存在，再 `INSERT`（而非 `INSERT ... ON CONFLICT DO NOTHING RETURNING`）。Broadcast 场景下多个 vtoken 并发处理同一 `real_ctx` 时，两个任务可能同时通过 `SELECT` 判定为"不存在"，然后都尝试 `INSERT`，触发唯一约束错误。
- **建议**：改为单条 `INSERT ... ON CONFLICT (real_ctx) DO NOTHING` + `SELECT vctx WHERE real_ctx = $1`，或 `INSERT ... ON CONFLICT (real_ctx) DO UPDATE SET real_ctx = real_ctx RETURNING vctx`。

### R-03 未知队列 backend 静默 fallback，配置错误不可见

- **严重度**：P1 / 可靠性
- **位置**：`src/runtime/serve.rs:292-314`
- **问题**：`redis` 和未知值都 silent fallback 到 memory，只打 warn/error 日志，不返回 `Err`。生产配置错误不会导致启动失败，而是悄悄使用内存队列，Hub 重启后消息全部丢失。
- **建议**：`redis` 和未知值返回 `Err(anyhow!("unsupported queue backend: {other}"))` 使进程在启动时明确失败。

### R-04 上游 long-poll 重登失败后缺少足够的等待间隔

- **严重度**：P2 / 可靠性
- **位置**：`src/ilink/upstream.rs:269-373`
- **问题**：`-14` 错误触发 QR 重登。重登失败后 `renewing.store(false)` 后下一轮会立刻再次以可能只有 1s 的 backoff 睡眠，可能导致重登失败的快速循环中发起过多 QR 请求。
- **建议**：重登失败后设置一个更长的固定等待间隔（如 30s）而非使用当前 backoff 值。

### R-05 `validate_env_ident` 的防御逻辑依赖上游 bail 生效

- **严重度**：P2 / 可靠性
- **位置**：`src/bridge/config.rs:578`
- **问题**：`chars().next().unwrap()` 虽然在调用前已检查 `ident.is_empty()`，但逻辑上依赖前一个 bail 生效，属于防御不足。
- **建议**：改为 `ident.chars().next().ok_or_else(|| anyhow!(...))` 或使用 `ident.as_bytes()[0]`。

---

## 三、安全

### S-01 未设 `ILINK_ADMIN_TOKEN` 时，管理端点完全开放（P0）

- **严重度**：**P0** / 安全
- **位置**：`src/server/routes.rs:37-65`，`src/server/mod.rs:48`（`CorsLayer::permissive()`）
- **问题**：`admin_token()` 返回 `None` 时 `check_admin_auth()` 直接返回 `true`，只有一条 warning 日志。Hub 监听 `0.0.0.0:8765`，若 `ILINK_ADMIN_TOKEN` 未设置，任何互联网用户都可以注册新虚拟 token、删除/重命名客户端、查看所有已注册客户端、触发 WeChat QR 重登。
- **建议**：将"无 token = 无鉴权"改为显式配置（`ILINK_ADMIN_INSECURE_NO_AUTH=true`），默认情况下若 `ILINK_ADMIN_TOKEN` 未设置，管理端点返回 503 或只允许本地回环访问。

### S-02 全局 `CorsLayer::permissive()` 扩大管理接口攻击面

- **严重度**：P1 / 安全
- **位置**：`src/server/mod.rs:48`
- **问题**：`CorsLayer::permissive()` 允许任意 `Origin`（含跨域脚本）以任意 HTTP 方法访问所有路由，包括无 token 保护的管理端点。任意网页上的 JavaScript 可跨域注册客户端。
- **建议**：仅对需要跨域的路由（如 `/ilink/bot/*`）设置 CORS，管理路由不设 CORS 或仅允许 `localhost`。

### S-03 `{{MESSAGE}}` 直接注入 CLI 参数，存在命令注入风险

- **严重度**：P1 / 安全
- **位置**：`src/bridge/mod.rs:523-538`，`src/bridge/mod.rs:429`
- **问题**：若用户配置 `command: bash` + `args: ["-c", "run --query {{MESSAGE}}"]`，用户输入可直接注入 shell 命令。虽然 `tokio::process::Command` 本身不通过 shell 执行，但此类配置形态的攻击面仍然存在。
- **建议**：文档明确警告不要将 `{{MESSAGE}}` 用于 shell 脚本的 `-c` 参数；推荐使用 `stdin: message` 模式作为安全替代；YAML 校验阶段检测高危模式（`command: bash/sh` + `-c` + `{{MESSAGE}}`）。

### S-04 `local_hostname()` 通过 fork `hostname` 命令获取主机名

- **严重度**：P2 / 安全
- **位置**：`src/bridge/connection.rs:204`
- **问题**：在 Docker 容器或受攻击的环境中，`hostname` 命令输出可能包含特殊字符，随后被拼接为 Hub 注册名。虽然 `sanitize_client_name_segment` 会过滤，但属于防御不足。
- **建议**：使用 `hostname` crate 或直接读 `/proc/sys/kernel/hostname`，避免 fork 子进程。

### S-05 `vtoken` 以明文写入结构化日志

- **严重度**：P2 / 安全
- **位置**：`src/server/routes.rs:139`
- **问题**：`warn!(vtoken = %vtoken, ...)` 将虚拟 token（客户端认证凭据）完整写入结构化日志，日志如果被收集到外部系统会造成凭据泄露。
- **建议**：对 `vtoken` 在日志中做截断（仅打印前 8 位 + `...`）；`WeixinMessage` 实现 `Debug` 时对 `context_token` 等字段做 redact 处理。

---

## 四、性能

### P-01 `InMemoryQueue` 持有全局单锁，高并发时完全串行化

- **严重度**：P1 / 性能
- **位置**：`src/hub/queue.rs:294-351`
- **问题**：当多个后端客户端同时 long-poll 时，每次 poll 都需要获取全局 `queues` Mutex 来查找自己的 `ClientQueue.notify`。N 个客户端并发 long-poll 时，吞吐量受限于单锁。
- **建议**：将 `ClientQueue` 改为 per-client 持有（注册时创建，getupdates 时直接引用），或使用 `dashmap::DashMap` 替代 `tokio::sync::Mutex<HashMap>`。

### P-02 Broadcast 路径每个 vtoken 触发多次 DB 操作

- **严重度**：P2 / 性能
- **位置**：`src/hub/mod.rs:299-347`
- **问题**：每条广播消息对每个 vtoken 触发：1 次 `persist_context_token` DB 写 + 2 次 DB 读（`get_active_session_name` + `get_backend_session`）。3 个后端 = 每条消息至少 7 次 DB 操作。
- **建议**：`persist_context_token` 合并为批量 upsert；`build_hub_ext_for_vctx` 的 DB 读加内存 LRU 缓存层（近期活跃 session），避免每条消息都读 DB。

### P-03 `HubState.ctx_map` 是 `Mutex`，每条消息独占锁

- **严重度**：P2 / 性能
- **位置**：`src/hub/mod.rs:75`
- **问题**：`ctx_map` 在 `dispatch_message`（写）和 `sendmessage` handler（先读后可能写）中频繁访问，Broadcast 时更是对每个 vtoken 各做一次 `lock().await`。
- **建议**：改用 `RwLock<ContextTokenMap>`（大多数 `resolve` 操作是只读的），或使用 `arc-swap`。

### P-04 `debug!` 宏中序列化完整 `item_list` JSON

- **严重度**：P2 / 性能
- **位置**：`src/ilink/upstream.rs:323-326`
- **问题**：使用 `%` 格式符时，即使日志级别不在 debug，表达式仍会被求值（序列化完整消息体）。
- **建议**：改用 `?` 格式符（`tracing` 的 lazy debug 格式），或将序列化放入闭包。

---

## 五、可观测性

### O-01 Prometheus metrics 只有 7 个指标，缺乏关键维度

- **严重度**：P2 / 可观测性
- **位置**：`src/server/routes.rs:747-829`
- **缺失指标**：上游 iLink 连接状态（gauge 0/1）、`sendmessage` 延迟分布（histogram）、DB 操作延迟/错误率、`context_token_map` 内存条目数量、每客户端消息速率、QR 重登次数。
- **建议**：引入 `metrics` crate，在各关键路径通过 `metrics::counter!`、`metrics::histogram!` 埋点，避免手写 metrics 与 `/metrics` 路由分散在两处。

### O-02 核心消息路径缺少贯穿 trace span

- **严重度**：P2 / 可观测性
- **位置**：`src/hub/mod.rs:dispatch_message`，`src/server/routes.rs:sendmessage`，`src/bridge/mod.rs:handle_one_message`
- **问题**：消息从 upstream 到 client 的完整路径（上游 poll → dispatch → queue push → getupdates drain → bridge handle → sendmessage → upstream send）没有贯穿的 trace span，无法通过 Jaeger/Zipkin 追踪单条消息的端到端延迟。
- **建议**：在 `dispatch_message` 入口添加 `#[tracing::instrument(skip(state), fields(from=...))]`，在 `sendmessage` 和 `getupdates` handler 中添加 span。

### O-03 `messages_dispatched` counter 在 drop 时重复计数

- **严重度**：P2 / 可观测性
- **位置**：`src/hub/mod.rs:229-237`
- **问题**：对于广播消息，`messages_dispatched` 在 `queue.push` 返回 `Ok(true)`（已 dropped）时**仍然加 1**，然后再加 `messages_dropped`。一条被 drop 的广播消息会在两个 counter 上各加 N（N = 在线客户端数）。
- **建议**：`messages_dispatched` 仅在 `Ok(false)`（真正进队成功）时递增；`messages_dropped` 在 `Ok(true)` 和 `Err` 时递增。

---

## 六、可维护性

### M-01 废弃表和列持续积累在 migrate() 中

- **严重度**：P2 / 可维护性
- **位置**：`src/store/mod.rs:105-130`
- **问题**：`backend_sessions`（v1）表在 v2 后不再读写，但仍每次启动时被 `CREATE TABLE IF NOT EXISTS`。`context_token_map.active_session_name` 列已通过 `active_sessions` 表取代，但 soft migration 的 `ALTER TABLE ... ADD COLUMN` 依然存在。
- **建议**：迁移到 `sqlx::migrate!` 宏，每个版本用独立 `.sql` 文件管理，彻底消灭废弃表/列的创建逻辑。

### M-02 `HubError::Upstream(#[from] anyhow::Error)` 语义模糊

- **严重度**：P2 / 可维护性
- **位置**：`src/error.rs:6`
- **问题**：`HubError` 既有 `thiserror` 的具体变体（`ClientNotFound`、`InvalidToken` 等），又有一个 `Upstream(anyhow::Error)` 作为 catch-all，调用方无法区分"上游 HTTP 失败"和"序列化失败"。
- **建议**：增加具体变体（`UpstreamHttp { code, msg }`、`UpstreamParse(serde_json::Error)` 等），去掉 `anyhow::Error` 作为 from 的 catch-all。

### M-03 关键异步集成路径缺少测试

- **严重度**：P2 / 可维护性
- **问题**：已有测试集中在纯函数（router 命令解析、quote_route 逻辑、bridge YAML 解析）。缺失：完整消息路由流（upstream → dispatch → queue → getupdates）、Broadcast 路径 N 个客户端分发、`sendmessage` 的 context_token 翻译、客户端注册/注销时路由状态一致性、DB 迁移幂等性验证。
- **建议**：使用 SQLite `:memory:` 数据库，为 `run_serve` 写集成测试，至少覆盖：单客户端消息路由、客户端离线后 fallback 为 broadcast、QR token 解析。

### M-04 中文 UI 字符串硬编码在业务逻辑文件中

- **严重度**：P2 / 可维护性
- **位置**：`src/hub/mod.rs:471`, `src/hub/mod.rs:540` 等多处
- **建议**：提取到 `messages.rs` 常量模块，与业务逻辑分离，方便后续多语言支持或文案修改。

---

## 七、依赖与配置

### D-01 `sqlx` 同时启用三个 driver（sqlite + postgres + mysql），二进制膨胀

- **严重度**：P2 / 依赖
- **位置**：`Cargo.toml:68`
- **问题**：即使大多数用户只用 SQLite，postgres 和 mysql 的 driver 代码（含 TLS 实现）都会编译进最终二进制，体积显著增大，也增加安全攻击面。
- **建议**：引入 cargo feature flags：`default = ["sqlite"]`，`postgres` 和 `mysql` 作为可选特性。

### D-02 `rand = "0.8"` 版本落后（当前稳定版为 0.9.x）

- **严重度**：P2 / 依赖
- **位置**：`Cargo.toml:65`
- **建议**：升级至 `rand = "0.9"`，更新调用处 `rand::thread_rng().gen()` → `rand::random::<u32>()`。

### D-03 `serde_yaml = "0.9"` 已停止维护

- **严重度**：P2 / 依赖
- **位置**：`Cargo.toml:72`
- **问题**：`serde_yaml` 0.9 于 2023 年停止维护，作者建议迁移至 `serde-yaml2` 或 `serde_norway`。
- **建议**：迁移至 `serde-yaml2`，或使用项目已有依赖 `config` crate 的 YAML 支持。

### D-04 `DefaultHasher` 用于 profile ID 生成，跨版本不稳定

- **严重度**：P2 / 安全
- **位置**：`src/bridge/manager.rs:571`
- **问题**：`std::collections::hash_map::DefaultHasher` 不保证跨平台/版本一致性（标准库明确说明实现可能变化）。Rust 版本升级后相同路径可能生成不同 ID，导致 credential 文件路径变化、bridge 重新自动注册。
- **建议**：改用 `fnv`、`xxhash` 或 SHA-256 截断（前 8 hex），保证跨版本稳定性。
