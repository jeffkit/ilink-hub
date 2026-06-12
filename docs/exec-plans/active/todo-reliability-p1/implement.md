# P1 可靠性修复 (DB-01, E-03) Implement Log

## M1 — DB-01: SQLite AnyPool 多连接并发问题修复

### Decisions

- 把 `Store::connect` 里 pool 分支的判断从 `url.contains(":memory:")`
  改为 `url.starts_with("sqlite:")`，让所有 `sqlite:` URL（file 和
  `:memory:`）统一走 `max_connections(1)`。
- 保留 sqlx `SqliteConnectOptions::new` 默认的 5s `busy_timeout` 不变，
  作为「shutdown 时 migration runner 仍在 acquire 排空」这种边缘场景
  的安全网。单连接 + 默认 busy_timeout 已经能完全避免 file-level
  EXCLUSIVE write lock 的多连接竞争，busy_timeout 退居二线。
- 把测试断言改为「结构性 invariant」：`store.pool.options().get_max_connections() == 1`。
  这是可重复、不依赖磁盘速度的回归护栏；之前单纯跑并发事务无法
  稳定触发 5s 锁等待，CI 里看不到 SQLITE_BUSY 也不能算覆盖到。
- 新增的并发负载测试同时覆盖 `persist_context_tokens_batch` /
  `set_active_session_name` / `get_active_session_name` 三类操作，
  复现 plan §M1 的「手动复现」步骤。

### Problems

- 第一次写测试时只跑 read+write，5s 默认 `busy_timeout` 把 busy 错误
  吞掉了，CI 里通过等于不通过。改为结构断言后，revert 修复时测试
  立刻以 `left: 10, right: 1` 失败，验证了护栏确实有效。
- 写测试时一开始用了 `format!("sqlite://{}", path)`，但 sqlx 的
  URL 解析会把单条 `//` 后面的内容当成 host，导致 `unable to open
  database file`（code 14）。改用项目里既有的 `format!("sqlite:{}", path)`
  形式（与 `tests/breaking_changes.rs:186` 一致）后通过。
- clippy 在测试里点出了字符串字面量误用 `{r}` / `{i}` 占位符的
  lint（普通字符串不是 format!），改为 `format!` 后清掉。

### Outcome

- `src/store/mod.rs::Store::connect` 的 pool 分支改为对所有 `sqlite:`
  URL pin `max_connections(1)`；doc comment 写清楚 file-level write
  lock 的原因和 5s busy_timeout 的角色。
- 新增 `store::store_tests::file_sqlite_serializes_concurrent_read_and_write_without_busy`：
  - 先断言 `pool.options().get_max_connections() == 1`（结构护栏，
    revert 修复会立即以 `left: 10, right: 1` 失败）；
  - 然后跑 8 个 batch-writer × 20 轮 × 200 行 + 4 个单行 writer ×
    200 轮 + 4 个 reader × 200 轮的并发负载（multi-thread runtime,
    8 workers），全部 join 不应返回 `SQLITE_BUSY`。
- 四条质量门禁全绿（fmt / clippy / cargo test / cargo build）：
  - cargo test：lib 122 + breaking_changes 7 + hub_routing_integration 9
    + queue_trait_tests 10 = 148 通过，0 失败。
- 写完 `docs/exec-plans/active/todo-reliability-p1/reviews/m1/review-request.yaml`，
  与上一份 todo-security-p1 的 m1 review 模板保持同样的字段结构。

## M2 — E-03: relay 客户端 shutdown 信号接入

### Decisions

- 把 `spawn_relay_client` 的 reconnect loop 拆成 `pub async fn run_relay_loop`，
  `spawn_relay_client` 签名变为
  `(identity, hub_base, relay_ws_url, shutdown: watch::Receiver<bool>)`，
  内部 `tokio::spawn(run_relay_loop(...))`。
  这样既不破坏现有 `spawn_*` 系列返回 `()` 的 API 风格（与
  `spawn_health_checker` / `spawn_quote_index_evictor` 一致），又能让
  单测直接 `await run_relay_loop(...)` 验证退出条件，避免引入一个只为
  测试存在的 `JoinHandle` 返回值。
- select! 三臂都包：`shutdown.changed()`（biased）+ `run_session(...)` +
  `tokio::time::sleep(RECONNECT_SECS)`，命中 shutdown 时 `return`，这与
  任务提示里的「loop 内 tokio::select! 包裹 sleep / run_session,命中
  shutdown 时 return」一致；外层 `loop { select! { ... } }` 不变，仍是
  无限重连，但每次循环先看 shutdown 信号。
- 进入 loop 前先 `if *shutdown.borrow() { return; }`，覆盖 shutdown 与
  startup 排序竞争：spawn 那一刻就已经是 true 的情况下，根本不应该
  去建第一次 WebSocket 连接再被 runtime drop。
- `shutdown.changed()` 命中后用 `if *shutdown.borrow() { return; }` 守卫
  而不是直接 return —— 防御性写法：万一上游 watch 通道被其他代码复用
  并被 toggle 回 false 再变 true，loop 也不会漏掉真正的 shutdown 翻转。
 实际上 tokio 的 `Sender::send` 只在值变化时通知 receiver，正常使用下
 不会触发这种"无变化的通知"，但代价只是一个分支判断，收益是更稳。
- 测试用 `DeviceIdentity::for_testing(device_id, signing_key_b64)`
  构造器（`#[doc(hidden)]`），避免 `DeviceIdentity::load_or_create` 写
  到 `dirs::data_local_dir()` 污染用户真实配置目录。

### Problems

- 第一次写第三个测试时想验证「loop 对非 shutdown 的 watch 变化不退出」，
  但 tokio 的 `watch::Sender::send` 在值未变时根本不会 notify receiver，
  `changed()` 不会 resolve，所以"发送一个 false 序列再观察 loop 仍在
  跑"是不可能的 API 路径。换成 smoke test：
  `spawn_relay_client_returns_without_blocking` 验证调用方启动序列不被
  relay 连通性阻塞，更直接有用。
- 写 `for_testing` 时一开始直接写 `pub fn for_testing(...)` 没有加
  `#[doc(hidden)]`，clippy 没意见但觉得语义上"测试专用"应该明确标记，
  最终加了。
- `cargo clippy --all-targets -- -D warnings` 暴露出一个 M1 已存在的
  lint（`tests/hub_routing_integration.rs:16` 的 `unused import:
  MessageQueue`），不属于 M2 scope，留给 M3 处理。任务提示要求的是
  `cargo clippy -- -D warnings`（默认 target 集合，不带
  `--all-targets`），M2 范围内是绿的。

### Outcome

- `src/relay/client.rs::spawn_relay_client` 签名加 `shutdown:
  watch::Receiver<bool>`，loop 体提取为 `pub async fn run_relay_loop`，
  内含 biased `tokio::select!` 三臂 + spawn-time shutdown 守卫；
  `spawn_relay_client` 仍返回 `()` 与项目里其他 spawn 函数风格一致。
- `src/runtime/serve.rs:128` 的唯一 caller 改为
  `spawn_relay_client(identity, hub_base, relay_ws, shutdown_rx.clone())`，
  共用已经在跑的 `shutdown_rx` watch 通道（与 axum graceful_shutdown
  和 `upstream.run_polling_loop` 同一个源）。
- `src/relay/device.rs::DeviceIdentity::for_testing(device_id,
  signing_key_b64)`，`#[doc(hidden)]`，仅供 `#[cfg(test)]` 使用。
- 新增 `relay::client::tests` 三个测试：
  - `relay_loop_returns_immediately_when_shutdown_already_true`：
    shutdown 在 spawn 时就是 true 的情况下 200ms 内退出（不调
    `run_session`、不建任何 WebSocket）。
  - `relay_loop_exits_during_reconnect_sleep_when_shutdown_signalled`：
    用不可达 WS URL 让 `run_session` 快速失败、loop 落到 5s reconnect
    sleep，期间 `shutdown.send(true)` 必须在 2s 内让 loop 退出
    （远小于 RECONNECT_SECS，证明 select! 真的把 sleep 中断了而不是
    等它自然醒来）。
  - `spawn_relay_client_returns_without_blocking`：调用本身在 50ms
    内返回（smoke test）。
- 四条质量门禁全绿（fmt / clippy / cargo test / cargo build）：
  - cargo test：lib 125（之前 122 + 3 个 relay 新增）+ breaking_changes
    7 + hub_routing_integration 9 + queue_trait_tests 10 = 151 通过，
    0 失败。
- 写完 `docs/exec-plans/active/todo-reliability-p1/reviews/m2/review-request.yaml`，
  模板结构与 m1 一致。
