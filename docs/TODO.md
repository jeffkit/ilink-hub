# iLink Hub — 待处理技术债务清单

> 生成日期：2026-06-12  
> 状态说明：`open` = 待处理，`in_progress` = 进行中，`done` = 已完成  
> 已完成项（本轮已修复）：R-01, R-02, R-03, P-01, P-04, A-02, A-03, D-03, O-03, D-04, S-02(CORS), S-01(admin token)

---

## 总览

| ID | 严重度 | 分类 | 简述 |
|----|--------|------|------|
| DB-01 | **P1** | 可靠性 | SQLite AnyPool 多连接导致 SQLITE_BUSY |
| SEC-001 | **P1/High** | 安全 | pair_confirm TOCTOU — vtoken 劫持 |
| SEC-003 | **P1/High** | 安全/DoS | getupdates 无并发限制 — 连接耗尽 |
| SEC-013 | **P1/High** | 安全 | pair_confirm 无认证 — 任意 vtoken 注册 |
| E-03 | P1 | 可靠性 | relay 客户端无 shutdown 信号 |
| POLL-01 | P2 | 可靠性 | -14 重登阻塞整个轮询 loop |
| LOCK-01 | P2 | 性能 | health checker 持 registry.write() O(N) 扫描 |
| LOCK-02 | P2 | 性能 | quote_index Mutex 序列化所有 dispatch 任务 |
| CHAN-01 | P2 | 可靠性 | SessionDispatcher 用 UnboundedSender，无背压 |
| CHAN-02 | P2 | 可靠性 | broadcast(256) 在 dispatcher 慢时静默丢消息 |
| MEM-01 | P2 | 性能 | Broadcast 路径 item_list 深 clone N 次 |
| SYNC-01 | P2 | 一致性 | 启动恢复的路由条目未验证 vtoken 有效性 |
| SYNC-02 | P2 | 一致性 | bridge 重注册同名新 vtoken 后路由表残留 |
| TO-01 | P2 | 可靠性 | handle_one_message 无整体超时 |
| TO-02 | P2 | 可靠性 | build_hub_ext_for_vctx DB 查询无超时 |
| DB-03 | P2 | 可移植性 | get_hub_ext_batch 行值 IN 语法不兼容 MySQL |
| SEC-002 | P2/Medium | 安全 | 配对码 Scanned→Confirmed 60s 重放窗口 |
| SEC-004 | P2/Medium | 安全/DoS | get_qrcode_status 无认证无限速 |
| SEC-005 | P2/Medium | 安全 | relay 注册无 nonce，60s 重放攻击 |
| SEC-007 | P2/Medium | 安全 | /metrics 无认证，泄露客户端名称 |
| SEC-008 | P2/Medium | 安全/DoS | relay device_keys/pending map 无界增长 |
| SEC-009 | P2/Medium | 安全 | name/label 字段无长度校验 |
| SEC-010 | P2/Medium | 安全/DoS | 所有路由无 HTTP body 大小限制 |
| SEC-012 | P2/Medium | 安全 | LRU 淘汰后 vctx 可被不同用户复用 |
| A-01 | P2 | 架构 | HubState 神对象 14 个字段无边界 |
| D-01 | P2 | 依赖 | sqlx 三驱动同时编译，二进制膨胀 |
| E-01 | P2 | 可靠性 | sendtyping 错误完全丢弃 |
| E-02 | P2 | 可靠性 | notify_start 响应被丢弃 |
| E-04 | P2 | 可靠性 | polling loop 最多延迟 70s 响应 shutdown |
| S-01 | P2 | 安全 | vtoken 在 debug! 日志中未 redact |
| S-02 | P2 | 安全 | vctx 完整值出现在 warn! 日志 |
| B-01 | P2 | 可靠性 | session worker CLI 崩溃后无退避 |
| API-01 | P2 | 可维护性 | HTTP 错误响应格式不统一 |
| T-01 | P2 | 测试 | relay 模块无任何测试覆盖 |
| C-01 | P2 | 一致性 | Broadcast persist fire-and-forget 重启丢失窗口 |
| S-03 | P2 | 安全/文档 | {{MESSAGE}} 注入风险缺 README 警告 |
| SEC-006 | P2/Low | 安全 | admin token 非常量时间比较 |
| SEC-011 | P2/Low | 安全 | relay path allowlist URL 编码绕过 |
| SEC-014 | P2/Low | 安全/DoS | InMemoryQueue slot 无总量上限 |
| MISC-01 | P3 | 性能 | resolve_full 用 peek 不更新 LRU 热度 |
| MISC-02 | P3 | 可靠性 | run_bridge 错误重试无退避 |
| MGR-03 | P3 | 性能 | BridgeApp::load 阻塞 async executor |
| TO-03 | P3 | 可靠性 | relay WebSocket 无单帧超时 |
| MEM-02 | P3 | 内存 | QuoteRouteIndex::by_content 无总量上限 |
| DB-02 | P3 | 性能 | persist_context_tokens_batch 事务随 N 增大 |
| POLL-02 | P3 | 可靠性 | 网络恢复后无 catch-up 逻辑 |
| MGR-01 | P3 | 可靠性 | 重启退避重置阈值等于最大退避值 |
| MGR-02 | P3 | 可靠性 | handle drop 检测延迟一个 reconcile 周期 |
| D-02 | P3 | 依赖 | rand 版本落后（0.8 → 0.9） |
| A-02 | P3 | 可配置性 | MAX_QUEUE_SIZE 硬编码不可调整 |
| T-02 | P3 | 测试 | pairing AlreadyConfirmed 路径未测试 |

---

## P1 — 需立即修复

### DB-01 · SQLite AnyPool 多连接导致 SQLITE_BUSY

- **状态**：done
- **文件**：`src/store/mod.rs:34-41`
- **问题**：文件型 SQLite 使用 `AnyPool::connect(url)` 默认会建立多个连接。SQLite 文件锁不允许多个写事务并发，当 `persist_context_tokens_batch`（写事务）和 `get_active_session_name`（读）在不同连接上并发时，会触发 `SQLITE_BUSY`（错误码 5）。`:memory:` 已正确 `max_connections(1)`，但文件型 SQLite 没有同等保护。
- **修复方向**：
  ```rust
  // 文件型 SQLite 同样 pin 到单连接，或设置 busy_timeout
  let pool = if url.contains(":memory:") || url.starts_with("sqlite:") {
      sqlx::pool::PoolOptions::<sqlx::Any>::new()
          .max_connections(1)
          .connect(url).await?
  } else {
      AnyPool::connect(url).await?
  };
  ```
  或通过连接参数 `sqlite:path/to/db?busy_timeout=5000` 设置 5 秒等待。

### SEC-001 · pair_confirm TOCTOU — vtoken 劫持（CWE-362）

- **状态**：done
- **文件**：`src/server/pairing.rs:381-386`
- **严重度**：High
- **攻击场景**：攻击者对同一配对码并发发送两个 `POST /hub/pair/{code}/confirm` 请求（不同 name），两个请求在 `pairing.confirm()` 之前都执行完 `register_client_in_hub`，导致第一个请求抢占到 vtoken，第二个请求得到 409。操作员的客户端永远拿不到正确的 vtoken——实质上是 vtoken 命名空间的会话劫持。
- **修复方向**：在单个写锁内原子完成「校验 code 状态 → register_client → confirm」三步，不允许中间状态被其他请求观察到。

### SEC-003 · getupdates 无并发限制 — 连接耗尽 DoS（CWE-307）

- **状态**：done
- **文件**：`src/server/routes.rs:141-226`，`src/hub/queue.rs:394-399`
- **严重度**：High
- **攻击场景**：持有有效 vtoken 的客户端可以开启任意数量的 60 秒长轮询连接。`PollTracker` 只打 warn 不阻断，没有每 vtoken 的并发上限。单个 vtoken 可以耗尽 Tokio worker 线程和连接池。
- **修复方向**：在 `getupdates` handler 中检查 `PollTracker` 的并发数，超过阈值（如 3）时立即返回 HTTP 429：
  ```rust
  let (count, _guard) = state.poll_tracker.enter(&vtoken);
  if count > MAX_CONCURRENT_POLLS {
      return (StatusCode::TOO_MANY_REQUESTS, ...).into_response();
  }
  ```

### SEC-013 · pair_confirm 无认证 — 任意 vtoken 注册（CWE-284）

- **状态**：done
- **文件**：`src/server/mod.rs:46-47`，`src/server/pairing.rs:362-413`
- **严重度**：High
- **攻击场景**：`POST /hub/pair/{code}/confirm` 无认证。配对码以 INFO 级别记录在日志中（`info!(code = %code, pair_url = %pair_url)`），任何能读取 Hub 日志的人可提取活跃码，调用该端点注册任意名称客户端并获得完整 vtoken，从此接收所有路由到该后端的微信消息。
- **修复方向**：
  1. 将 pair_url 日志降为 DEBUG 级别
  2. 强制要求 code 处于 `Scanned` 状态才允许 confirm（手机扫码是唯一合法触发路径）
  3. 在 pair HTML 页面中嵌入 CSRF token，confirm 请求必须携带才能通过

### E-03 · relay 客户端无 shutdown 信号，进程关闭时强制 kill

- **状态**：done
- **文件**：`src/relay/client.rs:18-29`
- **问题**：`spawn_relay_client` 生成的 task 是无限重连 loop，没有 `watch::Receiver<bool>` shutdown 信号。Hub 关闭时该 task 被 tokio runtime 强制 drop，relay 服务端看到异常断开。对比 `spawn_health_checker`、`spawn_quote_index_evictor` 均正确使用了 `shutdown.changed()` select 分支。
- **修复方向**：在 `spawn_relay_client` 签名中加入 `shutdown_rx: watch::Receiver<bool>` 参数，loop 内的 sleep 和 run_session 均用 `tokio::select!` 包裹，命中 shutdown 时 `return`。

---

## P2 — 安全问题

### SEC-002 · 配对码 Scanned→Confirmed 60 秒重放窗口（CWE-613）

- **状态**：done
- **文件**：`src/hub/pairing.rs:61-80`
- **问题**：配对码在 `Wait` → `Scanned` 转换后仍保有原始 600 秒 TTL，无次级截止时间。手机扫码到操作员确认之间存在最长 600 秒的窗口，观察到 QR 链接的第三方可以抢先调用 confirm。
- **修复方向**：进入 `Scanned` 状态时将 TTL 重置为 60 秒；`Confirmed` 后立即从 registry 删除该 code；限制同时存活的配对 session 数量（如 10 个）防止内存耗尽。

### SEC-004 · get_qrcode_status 无认证无限速 DoS（CWE-307）

- **状态**：done
- **文件**：`src/server/pairing.rs:299-316`
- **问题**：`GET /ilink/bot/get_qrcode_status?qrcode=<any>` 无认证，每请求最多持有 25 秒 Tokio task。攻击者可用随机 qrcode 值开启数千并发连接耗尽服务器。
- **修复方向**：对该端点应用 per-IP 限速（复用 relay 已有的 `RateLimiter` 模式），或在 loop 开始前验证 code 存在且处于活跃状态，不存在时立即返回 404。

### SEC-005 · relay 注册无 nonce，60 秒重放攻击（CWE-330）

- **状态**：done
- **文件**：`src/relay/auth.rs:7-39`，`src/relay/server.rs:105-106`
- **问题**：签名 payload 为 `"register:{device_id}:{timestamp}"`，时间窗口为 ±60 秒。攻击者截获一个有效的 Register WebSocket 帧后，可在 60 秒内向任意 relay 实例重放，伪装为合法 Hub 设备，劫持配对流量。
- **修复方向**：relay 服务端在 WebSocket 握手时发送 server nonce；Hub 签名 `"register:{device_id}:{timestamp}:{nonce}"`；或在服务端维护 `(device_id, timestamp)` 已用集合拒绝重放。

### SEC-006 · admin token 非常量时间比较（CWE-312）

- **状态**：done
- **文件**：`src/server/routes.rs:69`
- **当前**：`provided == required`（标准字符串比较，可能短路）
- **问题**：在同一局域网或共享云环境中，精确计时攻击可逐字节推断 admin token。
- **修复方向**：使用 `subtle` crate（已是 `ed25519-dalek` 的传递依赖）进行常量时间比较：
  ```rust
  use subtle::ConstantTimeEq;
  provided.as_bytes().ct_eq(required.as_bytes()).into()
  ```

### SEC-007 · /metrics 无认证，泄露客户端名称和消息量（CWE-284）

- **状态**：done
- **文件**：`src/server/mod.rs:53-54`，`src/server/routes.rs:825-933`
- **问题**：`GET /metrics` 无任何认证，暴露所有注册客户端名称（通过 Prometheus label）、队列深度、消息吞吐量、iLink 连接状态，可用于攻击者对部署进行指纹识别。
- **修复方向**：对 `/metrics` 应用 `check_admin_auth` 中间件，或通过独立内部端口暴露（推荐 Prometheus 最佳实践）。

### SEC-008 · relay device_keys/pending map 无界增长（CWE-400）

- **状态**：done
- **文件**：`src/relay/server.rs:43-46`
- **问题**：`device_keys: HashMap` 每次新 device_id 注册就增加一条，无 LRU 淘汰上限。攻击者用不同 keypair 不断注册可耗尽 relay 服务器内存。`RateLimiter` 只限制单 IP，IPv6 轮换可绕过。
- **修复方向**：为 `device_keys` 设置最大条目数（如 10,000），采用 LRU 策略淘汰最久未见的 device_id。

### SEC-009 · name/label 字段无长度校验（CWE-20）

- **状态**：done
- **文件**：`src/server/routes.rs:109-137`（`register`），`src/server/pairing.rs:362-413`（`pair_confirm`）
- **问题**：`name` 和 `label` 字段无长度限制，10MB 的 name 可以被存入内存和 SQLite。Prometheus metric label 中的 client name 未做特殊字符转义（大括号、换行），可破坏 Prometheus scrape 输出。
- **修复方向**：在两个 handler 的入口处验证 `name.len() <= 64`、`label.len() <= 256`，超出返回 HTTP 400；Prometheus 输出中对 client name 进行转义。

### SEC-010 · 所有路由无 HTTP body 大小限制（CWE-400）

- **状态**：done
- **文件**：`src/server/mod.rs:17-61`（router 定义处无 `RequestBodyLimitLayer`）
- **问题**：Axum 默认 body 限制为 2MB，但未显式配置 `DefaultBodyLimit`。`sendmessage` 接受含 `item_list`（嵌套 voice、text、binary payload）的请求体，relay 的 `body: String` 字段不做长度检查，可被恶意 relay 推送超大 body 到 Hub 本地 HTTP 栈。
- **修复方向**：在 `build_router` 中加入显式限制：
  ```rust
  .layer(DefaultBodyLimit::max(256 * 1024)) // 256KB 全局上限
  ```
  可对特定路由通过 `.layer(DefaultBodyLimit::disable())` 单独放开。

### SEC-011 · relay path allowlist URL 编码绕过（CWE-601）

- **状态**：done
- **文件**：`src/relay/device.rs:130-135`
- **问题**：`is_allowed_relay_path` 只检查字面量 `".."` 是否存在于 path 字符串。`%2e%2e` 或 `%2F..%2F` 形式的路径穿越序列可绕过该检查，通过 reqwest 的 URL 解码后到达 Hub 的内部 admin 端点。
- **修复方向**：对 path 进行 URL 解码后再次检查是否含 `..`；或在字符许可集（`[a-zA-Z0-9/_-]`）之外均拒绝，彻底禁止百分比编码。

### SEC-012 · LRU 淘汰后 vctx 可被不同对话复用（CWE-613）

- **状态**：done
- **文件**：`src/hub/queue.rs:64-178`，`src/store/mod.rs:573-593`
- **问题**：`ContextTokenMap` LRU 达到 50,000 上限后会淘汰旧条目。被淘汰的 peer 下次发来消息时，`map_scoped` 为其创建新 `vctx_<uuid>`，向 DB 写入时若 unique index on `real_ctx` 不存在（pre-v3 迁移数据库），两个不同 vctx 会映射到同一 real_ctx，可能导致 sendmessage 向错误用户发送消息。
- **修复方向**：在启动时断言 `idx_context_token_map_real_ctx` unique index 存在；`persist_context_token` 失败时（唯一约束冲突）记录 warn 而非静默忽略。

### SEC-014 · InMemoryQueue slot 无总量上限（CWE-400）

- **状态**：done
- **文件**：`src/hub/queue.rs:370-376`
- **问题**：`get_or_create` 为任意传入的 vtoken 字符串创建 slot，DashMap 无总条目上限。虽然 `getupdates` 路径有 registry 检查，但内部代码路径（如 dispatch 对已删除 vtoken 的 push）可绕过，长期运行会造成孤儿 slot 积累。
- **修复方向**：为 `InMemoryQueue` 增加最大 slot 数（如 1,000），超出时 `push` 返回 `Err(HubError::QueueBackend("queue slot limit exceeded"))`。

---

## P2 — 可靠性和性能问题

### POLL-01 · -14 重登阻塞整个轮询 loop，重登期间消息全部丢失

- **状态**：done
- **文件**：`src/ilink/upstream.rs:342-370`
- **问题**：iLink 返回 -14（会话过期）时，轮询 loop 原地等待 QR 重登完成（可能需要数分钟用户扫码）。重登期间轮询暂停，所有用户发来的消息均无响应，没有任何"服务暂时不可用"的回复。
- **修复方向**：将重登逻辑移到独立的 tokio task（已有 `relogin_tx` channel），主 polling loop 继续运行（每次 -14 触发 relogin task，loop 以更长退避等待），重登成功后 polling loop 用新 token 恢复。

### LOCK-01 · health checker 持 registry.write() 执行 O(N) 扫描

- **状态**：done
- **文件**：`src/hub/health.rs:27-31`
- **问题**：health checker 每 30 秒获取 `registry.write()` 锁，在持有写锁期间遍历所有客户端并修改 `online` 状态。写锁期间所有 `registry.read()` 调用（dispatch、getupdates、sendmessage）全部阻塞。客户端数量多时（N=100+）扫描可能耗时数毫秒，造成消息延迟抖动。
- **修复方向**：将 `ClientInfo.online` 改为 `Arc<AtomicBool>`，health checker 遍历时无需持有写锁，直接通过原子操作更新。

### LOCK-02 · quote_index Mutex 序列化所有 dispatch 任务

- **状态**：done
- **文件**：`src/hub/mod.rs:253-257`，`src/hub/quote_route.rs`
- **问题**：`dispatch_message` 对每条消息都获取 `quote_index.lock()`（`resolve_user_quote`），`tokio::sync::Mutex` 使所有并发 dispatch 任务在此串行化。`resolve_by_content` 目前接受 `&mut self` 但实际上只做查找，不需要 mut。
- **修复方向**：将 `resolve_user_quote`/`resolve_by_content` 改为 `&self` 方法；将 `quote_index` 改为 `RwLock<QuoteRouteIndex>`，dispatch 路径获取读锁（短暂持有），仅写入时获取写锁。

### CHAN-01 · SessionDispatcher 用 UnboundedSender，CLI 慢时无限积压

- **状态**：done
- **文件**：`src/bridge/mod.rs:161-218`
- **问题**：每个 session worker 持有一个 `UnboundedSender`，消息以无限速率进入，但 CLI 处理每条消息可能耗时数分钟。当 CLI 持续慢速处理时，channel 内积压消息无限增长，内存持续上升，最终 OOM。
- **修复方向**：改用有界 channel（如容量 10），`SessionDispatcher::dispatch` 发送失败时记录 warn 并丢弃最旧消息，与 `InMemoryQueue` 的背压策略保持一致。

### CHAN-02 · broadcast(256) 在 dispatcher 处理慢时静默丢消息

- **状态**：done
- **文件**：`src/runtime/serve.rs:90`，`src/hub/mod.rs:330`
- **问题**：`broadcast::channel::<WeixinMessage>(256)` 容量为 256。`spawn_dispatcher` 的 `dispatch_message` 包含 DB 查询（`get_hub_ext_batch`、`persist_context_tokens_batch`）。若 DB 慢（如 SQLite 锁争用），dispatcher 处理速度低于上游推送速度，超出 256 条后广播通道触发 `RecvError::Lagged`，消息被静默丢弃，只打一条 warn 日志。高峰期或 DB 慢时这会造成不可见的消息丢失。
- **修复方向**：增加通道容量（如 1024）；更重要的是将 DB 操作从 dispatcher 热路径移出（已有批量接口，继续优化将 DB 写入合并为后台 task）；并为 Lagged 事件增加 metrics counter，使丢失可观测。

### MEM-01 · Broadcast 路径 item_list 深 clone N 次（N = 在线客户端数）

- **状态**：done
- **文件**：`src/hub/mod.rs:369`（`msg_clone = msg.clone()`）
- **问题**：`WeixinMessage.item_list: Option<Vec<MessageItem>>` 在 Broadcast 路径中对每个在线客户端执行一次完整深 clone，`MessageItem.extra: serde_json::Value` 是 heap 分配的树结构，clone 代价高。3 个后端时深 clone 3 次，10 个后端时 10 次。
- **修复方向**：将 `item_list` 改为 `Option<Arc<Vec<MessageItem>>>`，clone 只复制 Arc 引用而非数据。需同步修改 `sendmessage` handler 中修改 `item_list` 的代码（写时复制）。

### SYNC-01 · 启动恢复的路由条目包含已删除 vtoken

- **状态**：done
- **文件**：`src/runtime/serve.rs:257-268`
- **问题**：`load_clients_from_db` 先恢复 clients，后恢复 routing_state。若某个 client 被删除但对应的 routing_state 行未清理（`clear_routes_for_vtoken` 仅在内存注销时调用，进程崩溃时可能跳过），重启后 `router` 内存中存在指向已删 vtoken 的路由，消息会被路由到不存在的 client 并静默丢弃。
- **修复方向**：`load_clients_from_db` 恢复路由后，过滤掉 registry 中不存在的 vtoken 对应的路由条目；或在 `upsert_client` 时同步清理同 vtoken 的旧路由。

### SYNC-02 · bridge 重注册同名新 vtoken 后 routing_state 残留旧 vtoken

- **状态**：done
- **文件**：`src/store/mod.rs:195-213`（`upsert_client`）
- **问题**：`upsert_client` 在 `ON CONFLICT (name) DO UPDATE SET vtoken = EXCLUDED.vtoken` 时更新了 clients 表的 vtoken，但 `routing_state` 表中 `active_vtoken` 字段指向旧 vtoken 的行不会被同步更新。bridge 重启后用新 vtoken 注册，原来通过 `/use` 选择该 client 的用户路由会指向旧 vtoken，消息路由失败。
- **修复方向**：`upsert_client` 执行后，在同一事务中执行 `UPDATE routing_state SET active_vtoken = $new_vtoken WHERE active_vtoken = $old_vtoken`。

### TO-01 · handle_one_message 无整体超时，慢 CLI 永久阻塞 session worker

- **状态**：done
- **文件**：`src/bridge/mod.rs:319-401`
- **问题**：`handle_one_message` 包含：① 等待 CLI 进程完成（有 `cli_timeout_secs` 子超时）；② 调用 Hub `sendmessage` HTTP（无超时）。若 Hub 宕机或网络超时，`sendmessage` 可能永久挂起，整个 session worker 被阻塞，后续消息无法处理。
- **修复方向**：在 `run_session_worker` 中用 `tokio::time::timeout` 包裹整个 `handle_one_message` 调用，超时时间建议为 `cli_timeout_secs + 30s`（给 sendmessage 留余量）。

### TO-02 · build_hub_ext_for_vctx DB 查询无超时

- **状态**：done
- **文件**：`src/hub/mod.rs:808-838`（`build_hub_ext_for_vctx`）
- **问题**：`get_active_session_name` 和 `get_backend_session` 两次 DB 查询没有超时保护。若 DB 因 SQLITE_BUSY 或 PostgreSQL 连接池耗尽而挂起，`dispatch_message` 任务永久阻塞，积压所有后续消息。
- **修复方向**：用 `tokio::time::timeout(Duration::from_secs(5), ...)` 包裹两次 DB 查询，超时时 warn 并返回 `None`（降级为无 HubExt 的消息转发）。

### DB-03 · get_hub_ext_batch 行值 IN 语法不兼容 MySQL

- **状态**：done
- **文件**：`src/store/mod.rs:432-479`
- **问题**：`WHERE (vctx, vtoken) IN (($1,$2), ($3,$4), ...)` 的行值构造语法在 SQLite 3.15+ 和 PostgreSQL 支持，但 MySQL 5.x 不支持（MySQL 8.0 支持）。若用户使用 MySQL 5.7，该查询直接报错导致 Broadcast 路径的 HubExt 全部降级为 None。
- **修复方向**：改为等价的 `OR` 子句：`WHERE (vctx = $1 AND vtoken = $2) OR (vctx = $3 AND vtoken = $4) OR ...`；或检测数据库类型选择不同 SQL。

### E-01 · sendtyping 错误被完全丢弃，客户端永远收到假成功

- **状态**：done
- **文件**：`src/server/routes.rs:439`
- **当前**：`let _ = state.upstream.send_typing(req).await;`
- **修复方向**：
  ```rust
  match state.upstream.send_typing(req).await {
      Ok(_) => Json(serde_json::json!({"ret": 0})),
      Err(e) => {
          warn!(error = %e, "send_typing upstream error");
          Json(serde_json::json!({"ret": 500, "errmsg": format!("upstream error: {e}")}))
      }
  }
  ```

### E-02 · notify_start 响应被丢弃，无法检测 iLink 业务层启动错误

- **状态**：done
- **文件**：`src/ilink/upstream.rs:77-84`
- **问题**：`notify_start` 响应体被 `let _` 丢弃，无法检测 iLink 返回的 -14 等业务错误码。若 notifystart 失败，所有后续 `sendmessage` 都会被 iLink 拒绝，但 Hub 不会感知。
- **修复方向**：解析响应体，`ret != 0` 时 `warn!` 并记录错误码；`ret == -14` 时触发重登流程。

### E-04 · polling loop 最多延迟 70 秒才响应 shutdown

- **状态**：done
- **文件**：`src/ilink/upstream.rs:271-275`
- **修复方向**：
  ```rust
  tokio::select! {
      biased;
      _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
      result = self.get_updates(buf.clone(), None) => { /* 原逻辑 */ }
  }
  ```

### S-01 · vtoken 在 debug! 日志中未经 redact 完整输出

- **状态**：done
- **文件**：`src/hub/router.rs:159`
- **修复方向**：`vtoken = %&vtoken[..vtoken.len().min(8)]`

### S-02 · vctx 完整值出现在 warn! 级别日志

- **状态**：done
- **文件**：`src/server/routes.rs:298, 302, 345`
- **修复方向**：`vctx = %&vctx[..vctx.len().min(8)]`

### B-01 · session worker CLI 崩溃后无退避，连续失败可能触发 spawn 风暴

- **状态**：done
- **文件**：`src/bridge/mod.rs:161-173`
- **修复方向**：连续失败时加指数退避（1s→2s→4s，上限 30s），或连续失败 N 次后让 worker 退出触发重建。

### API-01 · HTTP 错误响应格式不统一（ret/error/errmsg 混用）

- **状态**：done
- **文件**：`src/server/routes.rs`（多处）
- **修复方向**：为 admin API 引入统一错误结构体，或在文档中明确说明两类 API 的错误格式约定。

### T-01 · relay 模块核心逻辑无任何测试覆盖

- **状态**：done
- **文件**：`src/relay/client.rs`，`src/relay/server.rs`
- **修复方向**：为 `is_allowed_relay_path`（安全关键的白名单逻辑）添加参数化单元测试；为 `forward_to_hub` 超时行为添加集成测试。

### C-01 · Broadcast persist fire-and-forget 存在重启丢失窗口

- **状态**：done
- **文件**：`src/hub/mod.rs:349-354`
- **修复方向**：接受现有语义时，至少添加 metrics counter 记录 fire-and-forget 失败次数；在 README 中说明该设计权衡。

### S-03 · {{MESSAGE}} 注入风险缺少 README 静态警告

- **状态**：done
- **文件**：`README.md` 或 `docs/bridge-config.md`
- **修复方向**：在 `{{MESSAGE}}` 配置示例旁添加安全警告框，说明不要用于 shell `-c` 参数，推荐 `stdin: message` 模式。

### A-01 · HubState 神对象，14 个字段无访问边界

- **状态**：done
- **文件**：`src/hub/mod.rs:131-155`
- **修复方向**：按职责拆分为 `IlinkConnState`、`RoutingState` 等子结构，通过字段访问而非直接暴露全部状态。

### D-01 · sqlx 三驱动同时编译，二进制体积和攻击面增大

- **状态**：done
- **文件**：`Cargo.toml:70`
- **修复方向**：引入 `[features]` 并设 `default = ["sqlite"]`，postgres/mysql 作为可选特性。注意这是 **breaking change**，需配合文档更新。

---

## P3 — 改进建议

### MISC-01 · resolve_full 用 peek 不更新 LRU 热度，活跃会话可能被提前淘汰

- **状态**：done
- **文件**：`src/hub/queue.rs:152-161`
- **问题**：`resolve_full`（sendmessage 路径用于 reply 路由）用 `peek`（不更新 LRU 顺序），而 `resolve` 用 `get`（更新顺序）。结果是 sendmessage 的活跃会话不被计为"热"，在内存压力下反而更早被淘汰。
- **修复方向**：将 `resolve_full` 改为 `&mut self`，使用 `get` 正确更新 LRU；调用方持 write lock 即可（sendmessage 路径已有 write lock）。

### MISC-02 · run_bridge 错误重试无退避，Hub 宕机时持续轰炸

- **状态**：done
- **文件**：`src/bridge/mod.rs:249-253`
- **当前**：固定 3 秒重试，无指数退避，无 jitter
- **修复方向**：仿照 `run_polling_loop` 实现指数退避（3s → 6s → 12s，上限 60s）。

### MGR-03 · BridgeApp::load 在 async 任务中调用阻塞 I/O

- **状态**：done
- **文件**：`src/bridge/manager.rs:554`，`src/bridge/config.rs:194`
- **问题**：`std::fs::read_to_string` 是阻塞调用，在 `reconcile_once`（async fn）中直接调用会阻塞 Tokio worker 线程。
- **修复方向**：`tokio::task::spawn_blocking(|| BridgeApp::load(path)).await`

### TO-03 · relay WebSocket 读写无单帧超时

- **状态**：done
- **文件**：`src/relay/client.rs:59, 118`
- **问题**：relay 的 WebSocket `sink.send()` 和 `stream.next()` 无超时，网络半开连接时会永久挂起。
- **修复方向**：用 `tokio::time::timeout(Duration::from_secs(30), ...)` 包裹每次帧操作。

### MEM-02 · QuoteRouteIndex::by_content 无总量上限

- **状态**：done
- **文件**：`src/hub/quote_route.rs:77-105`
- **问题**：`by_content` HashMap 无总条目上限，每条 Hub 命令回复（list/status/session list 等）都会注册一条内容索引。高频使用 Hub 命令时内存持续增长。
- **修复方向**：在 `register_outbound_content` 中检查总条目数，超过上限（如 10,000）时跳过注册并打 warn。

### DB-02 · persist_context_tokens_batch 事务持有时间随 N 增大

- **状态**：done
- **文件**：`src/store/mod.rs:598-624`
- **问题**：在 Broadcast 场景下，事务持有时间 = N × 单条 upsert 耗时，N 较大时 SQLite write lock 被长期占用，阻塞其他写操作。
- **修复方向**：将批量写入分块（每批 50 条），或改用 `INSERT ... VALUES (...),(...),(...) ON CONFLICT DO UPDATE` 多值批量语法，减少事务往返次数。

### POLL-02 · 网络恢复后无 catch-up 逻辑

- **状态**：done
- **文件**：`src/ilink/upstream.rs:372-378`
- **问题**：polling loop 断线后以退避重连，重连成功后从当前时间点开始 poll，断线期间累积的消息可能已被 iLink 丢弃（取决于 iLink 服务端保留策略）。
- **修复方向**：记录最后成功 poll 的时间戳，重连后优先请求历史消息（若 iLink API 支持）；或在断线期间向用户发送"服务暂时中断，消息可能有延迟"的通知。

### MGR-01 · 重启退避重置阈值等于最大退避，快速崩溃绕过退避

- **状态**：done
- **文件**：`src/bridge/manager.rs:512-515`
- **问题**：重启退避在子进程存活时间超过一定阈值后重置为初始值。若该阈值等于最大退避值（如 60 秒），进程在每次退避结束后立刻崩溃（存活 < 1s），但退避计时器恰好在上一次等待中消耗完，下次会从头开始，无法真正惩罚持续快速崩溃。
- **修复方向**：将"健康存活"阈值设为显著大于最大退避（如最大退避的 3 倍），确保进程需要真正稳定运行一段时间才能重置退避计数。

### MGR-02 · handle drop 检测延迟一个 reconcile 周期

- **状态**：done
- **文件**：`src/bridge/manager.rs:198-212`
- **问题**：`BridgeManagerHandle` drop 后，manager 需要等到下次 `reconcile_once` 轮询时才检测到，最多延迟一个轮询间隔（默认数秒）。期间子进程仍在运行并接收消息。
- **修复方向**：使用 `tokio::sync::watch::Sender`，handle drop 时发送关闭信号，manager 在 `tokio::select!` 中同时等待该信号和定时器，做到即时响应。

### D-02 · rand 版本落后（0.8 → 0.9）

- **状态**：done
- **文件**：`Cargo.toml:64`
- **修复方向**：升级 `rand = "0.9"`；`rand::thread_rng().gen::<u32>()` → `rand::random::<u32>()`；检查 `ed25519-dalek` 的 `rand_core` 兼容性。

### A-02 · MAX_QUEUE_SIZE 硬编码，慢后端场景下不可调整

- **状态**：done
- **文件**：`src/hub/queue.rs:21`
- **修复方向**：在 `build_queue_backend()` 中读取 `ILINK_MAX_QUEUE_SIZE` 环境变量，有效范围 `[10, 10_000]`，超界时 warn 并 clamp。

### T-02 · pairing 测试未覆盖 AlreadyConfirmed 路径

- **状态**：done
- **文件**：`src/hub/pairing.rs:115-117`
- **修复方向**：添加测试：连续两次 confirm 同一 code，断言第二次返回 `AlreadyConfirmed`。

---

## 已完成的修复记录

| 问题 | 修复内容 |
|------|---------|
| R-02 TOCTOU 竞态 | `map_context_token` 改为 `INSERT ... ON CONFLICT DO NOTHING` + SELECT |
| R-03 配置错误 fallback | `build_queue_backend` 未知 backend 返回 `Err` 使进程失败 |
| P-01 全局锁 | `InMemoryQueue` 从 `tokio::sync::Mutex<HashMap>` 改为 `DashMap` |
| R-01 无界内存 | `ContextTokenMap` 四个 HashMap 改为 `LruCache`，上限 50,000 条 |
| P-04 debug 无条件序列化 | 删除 `item_list = ?msg.item_list` 字段 |
| A-02 dispatch_message 重复 | 提取 `push_to_queue()` 辅助函数 |
| A-03 handle_hub_command 样板 | 提取 `resolve_vctx_and_vtoken()` 辅助函数 |
| D-03 serde_yaml 停止维护 | 替换为 `serde_norway = "0.9.42"`（完全兼容的维护分支） |
| O-03 metrics 计数错误 | ForwardTo/Broadcast 的 dispatched/dropped 计数逻辑已正确 |
| D-04 DefaultHasher 不稳定 | 已改用 fnv1a32 |
| S-02(CORS) 全局 permissive | CORS 已仅作用于 `bot_api`，admin 路由不带 CORS |
| S-01(admin token) 无认证开放 | 已改为需显式设 `ILINK_ADMIN_INSECURE_NO_AUTH=true` |
