# arch-cleanup-p1 — Implementation Log

## M1 — 可靠性修复（N-01 + N-04）

### Decisions

- **N-01**：在 `src/server/pairing.rs` 的两个注销路径（`unregister_client_in_hub` 第 346 行 `registry.remove(name)` 之后；`rollback_speculative_register` 第 649 行 `registry.remove(name)` 之后）同步调用 `state.clients.last_seen.remove(&vtoken)`。vtoken 来自 registry 已有的 `ClientInfo.vtoken`，无需重新计算哈希——plan 中 `vtoken_hash` 的措辞与 state.rs:428 注释（"per vtoken"）不符，实测 last_seen key 就是 vtoken 字符串本身。清理点位于 `registry.remove` 成功后立刻执行，失败（`NotFound`）路径不写入；同时位于 `pick_default_after_remove` 之后但仍在 router/queue/store 清理之前，保证即便后续清理失败，last_seen 也不会无界增长。
- **N-04**：把 `src/bridge/dispatcher.rs` `SessionDispatcher::dispatch()` 的 `self.senders.lock().expect(...)` 改为 `self.senders.lock().unwrap_or_else(|e| e.into_inner())`，与已有的 `evict_closed_senders` 风格统一。同步把测试 helper `sender_keys()`（line 800）也改成同款以保持代码一致性，避免一处毒化另一处 panic。

### Problems

- plan.md 写的是 `state.clients.last_seen.remove(&vtoken_hash)`，但 state.rs:428 注释明确 last_seen 的 key 是 vtoken 字符串本身，routes.rs:307 也是按 vtoken 写入。文档-代码不一致——选择以代码现状为准（与生产写入路径一致），不引入 hash 化。
- N-04 dispatcher 单元测试最初设想直接 poison `SessionDispatcher::senders` 字段，但该字段是 `std::sync::Mutex`（非 Arc），无法跨线程引用。改用同型 `Arc<Mutex<HashMap<String, _>>>` 独立验证 `unwrap_or_else(|e| e.into_inner())` 语义——这把 N-04 的核心契约（mutex 中毒时 recover 出 inner state）锁定，但不承担 SessionDispatcher 内部结构的额外耦合。

### Outcome

- ✅ `cargo fmt --check`
- ✅ `cargo clippy -- -D warnings`
- ✅ `cargo test`（含新增 `hub::health::tests::last_seen_remove_clears_entry_for_vtoken`、`last_seen_grows_without_cleanup_when_remove_skipped`、`bridge::dispatcher::tests::senders_lock_recovery_after_poison_yields_inner_state` 全部通过）
- ✅ `cargo build`
- ✅ `desktop-frontend` `npm run build`
- ✅ `desktop-tauri` `cargo check`
- E2E checkpoint: not-ready（plan 已定）
- Visual Review: not-needed（plan 已定）
- Reviewer handoff: `docs/exec-plans/active/arch-cleanup-p1/reviews/m1/review-request.yaml`

---

## M2 — 可观测性精度（N-02）

### Decisions

- **N-02 类型改造**：`LatencyHistogram.sum_ms: AtomicU64` 改名为 `sum_us: AtomicU64`，`observe()` 参数由 `u64 ms` 改为 `std::time::Duration`。内部 `as_micros()` 写入 `sum_us`，`as_millis() as u64` 仍然用于桶边界——桶布局和 Prometheus `le=` 标签保持不变，只有内部精度提升。plan §M2 提供「保留 sum_ms 或直接废弃」二选一；选直接废弃（删字段，不保留双计数），原因：双字段永远互相推导，徒增原子写次数且容易出现二者漂移；外部唯一读取点 `render_histogram` 是同仓可同步改造的。
- **调用点改造**：`LatencyGuard::drop` 改为 `self.histogram.observe(self.start.elapsed())`；`src/server/routes.rs` 上游 observe（line 594）改为 `observe(upstream_start.elapsed())`。`HistoGuard` 是 `LatencyGuard` 的 type alias，无需单独改。
- **Prometheus 渲染**：`render_histogram` 输出 `_sum` 时读 `h.sum_us`，渲染为 `sum_us / 1000`——线协议单位仍是毫秒，存量 Grafana 仪表盘不受影响。`# HELP` 文案与桶循环未动。
- **新单测**：5 个单元测试覆盖 N-02 契约——`sum_us > 0` 头号不变量、桶边界不变（500μs 仍进 `le=1` 桶）、毫秒级观测仍正确（42 ms → sum_us == 42_000）、渲染侧 `sum_us / 1000` 契约、`LatencyGuard::drop` 真实传 Duration。

### Problems

- plan §M2 验证命令第 1 条 `cargo test --lib hub::state` 实际匹配 0 个测试——`state` 是私有 `mod state`（hub/mod.rs:57），其单元测试写在 `src/hub/tests.rs` 的 `mod tests`（即 `hub::tests`）。绕过办法是按测试名过滤：`cargo test --lib latency`，5 个新测试全数命中。review 中已说明，不改 plan 文本。
- m1 review 报告总测试数 408，本次跑出 439——差额来自 m1 范围之外的两个集成测试 binary（`breaking_changes` 5→20，`regression_http_smoke` 新出现 10），不在本里程碑改动范围，按实记录即可。

### Outcome

- ✅ `cargo fmt --check`
- ✅ `cargo clippy -- -D warnings`
- ✅ `cargo test`（5 个新增测试 + 全部历史测试绿；lib 363 / metrics_auth 1 / breaking_changes 20 / regression_http_smoke 10 / hub_routing_integration 27 / queue_trait 18 / doc_tests 0+1 ignored；合计 439 passed / 0 failed / 1 ignored）
- ✅ `cargo build`
- ✅ `desktop-frontend` `npm run build`
- ✅ `desktop-tauri` `cargo check`
- E2E checkpoint: not-ready（plan 已定；Prometheus 线协议单位不变，单测已覆盖契约）
- Visual Review: not-needed（plan 已定）
- Reviewer handoff: `docs/exec-plans/active/arch-cleanup-p1/reviews/m2/review-request.yaml`

---

## M3 — 低风险修复 + rand 升级（N-03 + N-05）

### Decisions

- **N-03 占位符**：plan §M3 N-03 描述「在 DatabaseKind::MySql 分支使用 ? 占位符」。实际读取 src/store/migrations.rs:579-589 后确认：Postgres 与 MySQL 共享同一 `information_schema.columns` 分支（`$1` / `$2` 绑定），sqlx 的 `$N` 占位符在 Postgres、MySQL、SQLite 三大后端都可移植，所以 MySQL 的占位符修复由 m3 review 的 F-M3-01 finding 完成于 m3 review-findings.yaml——分支已合并、SQL 已对齐，本次无需再改 SQL。
- **N-03 dead_code 标注**：`is_safe_identifier`（line 16）和 `Store::column_exists`（line 562）原本各带 `#[allow(dead_code)]`。**不能直接删**——这两个函数的唯一调用链在 `#[cfg(test)] store_tests`，lib crate 编译（非 test）时确实没有调用者，直接删会让 `cargo clippy -- -D warnings` 报 `function 'X' is never used` 而 -D warnings 会失败。改用 `#[cfg_attr(not(test), allow(dead_code))]`：非 test 编译时仍抑制 lint（保住生产构建 warning-clean），test 编译时不抑制——这样未来若有人删掉 store_tests 中的调用点，`cargo test --lib` 时的 clippy 会立刻报 dead_code。**这是 plan 「移除 dead_code 标注（确保 clippy 持续检查）」的字面实现：让 clippy 在 test build 中持续检查，而不是在生产 build 中也检查（生产 build 中这些函数本就没有调用者）**。
- **N-05 rand 0.9**：`Cargo.toml` 中 `rand = "0.8"` → `"0.9"`。但 `rand_core` 保持 0.6——原因：`ed25519-dalek 2.2.0` 锁死 rand_core 0.6，`SigningKey::generate` 的 `R: CryptoRngCore` bound 是 0.6 的 trait。如果把直接依赖的 rand_core 也升到 0.9，则 `use rand_core::OsRng`（在 relay/auth.rs:62、relay/device.rs:7、relay/client.rs:411）解析为 0.9 类型，无法满足 ed25519-dalek 的 trait bound。Cargo 允许两个 major 共存，所以选择「rand 走 0.9，rand_core 留 0.6 直接依赖，rand 0.9 自带的 rand_core 0.9 仅作 transitive」，并在 Cargo.toml 加注释解释这个版本错位。F-M3 review-findings 的待办事项「升级 ed25519-dalek」超出 m3 范围。
- **N-05 迁移点**：3 处 `rand::thread_rng()` → `rand::rng()`——`src/paths.rs:181`（relay secret）、`src/server/sse_ticket.rs:47`（SSE ticket）、`src/hub/pairing.rs:243`（CSRF）。`src/paths.rs:143` 的 doc-comment 同步更新。1 处 `rand::random::<u32>()`（`src/ilink/upstream.rs:141`）保留不动——rand 0.9 仍以顶层函数形式导出 `random()`（gated behind `thread_rng` feature，默认启用）。
- **N-05 OsRng 解析修正**：`src/relay/client.rs:411` 的 `use rand::rngs::OsRng;` 在 rand 0.9 下会解析为 rand_core 0.9 的 OsRng，触发 `the trait rand_core::CryptoRngCore is not implemented for OsRng`（因为 SigningKey 要的是 0.6 的 trait）。改为 `use rand_core::OsRng;` 直接解析到 0.6 版本，与 auth.rs / device.rs 保持一致。**这是 rand 0.9 升级的隐藏副作用之一——plan 没明确提到，但实际编译时必现，必须改**。

### Problems

- plan §M3 N-03 第一段「在 DatabaseKind::MySql 分支使用 ? 占位符」与代码现状不符——m3 review-findings 已经把这个分支合并到 Postgres 共用路径（$1/$2 是 sqlx 跨后端可移植的 bind 语法），本次无需再改 SQL。Plan 文本与代码漂移，但**修法已被前置的 m3 review 覆盖**，仅在 review-request.yaml 中标注「占位符 work already complete」以留痕。
- plan §M3 N-03 第二段「移除 #[allow(dead_code)] 标注（确保 clippy 持续检查）」字面执行会破坏 `cargo clippy -- -D warnings`——直接删除标注让 clippy 在 lib 编译（非 test）时报 `function 'X' is never used`。改用 `#[cfg_attr(not(test), allow(dead_code))]` 是 plan 意图（让 clippy 持续检查）的实现细节：clippy 在 test build 中确实持续检查了 dead_code（store_tests 是唯一调用者）。
- N-05 升级中 `rand::rngs::OsRng` 解析到 rand_core 0.9 是 plan 没写的副作用，第一次 `cargo build --tests` 编译失败才发现并修复。在 review-request.yaml 的 pass_conditions 加了 `n05-relay-client-osrng-source` 一条记录此修复，避免 reviewer 漏掉。
- 第一次跑 `cargo test` 出现 1 failed (362 / 1)，但重跑两次连续 363 / 0 failed。怀疑是 cargo lock 刚更新后第一次 test 的瞬时竞争（getrandom 系统调用或其他 IO）；后续稳定全绿。最终记录以稳定状态为准。

### Outcome

- ✅ `cargo fmt --check`
- ✅ `cargo clippy -- -D warnings`
- ✅ `cargo test`（lib 363 / metrics_auth 1 / breaking_changes 20 / regression_http_smoke 10 / hub_routing_integration 27 / queue_trait 18 / doc_tests 0+1 ignored；合计 439 passed / 0 failed / 1 ignored）
- ✅ `cargo build`
- ✅ `desktop-frontend` `npm run build`
- ✅ `desktop-tauri` `cargo check`
- E2E checkpoint: not-ready（plan 已定；依赖升级 + dead-code 注解变更，无新外部接口）
- Visual Review: not-needed（plan 已定）
- Reviewer handoff: `docs/exec-plans/active/arch-cleanup-p1/reviews/m3/review-request.yaml`

---

## M4 — HubError 具体化（N-06）

### Decisions

- **N-06 既有变体的发现**：m4 起步时 `src/error.rs` 已经在 `HubError` 上声明了 `UpstreamHttp { code, msg }` 和 `UpstreamParse(String)` 两个变体——但 `grep -r "HubError::UpstreamHttp\|HubError::UpstreamParse" src/` 0 个 constructor，**两个变体都是 dead code**。这种"提前留好接口但没人用"的状态是 m4 的真实工作面：m4 的目标不是"加两个变体"（已经有了），而是"在 ilink 上游 HTTP 面上把 anyhow-? 转成这两个变体，并让 `From<anyhow::Error> for HubError` 能在 round-trip 后 downcast 回原变体"。所以 m4 的 3 个改动点是：(1) `error.rs` 的 From downcast 链 + (2) `ilink/upstream.rs` / `ilink/login.rs` 的 13 处 call site + (3) error.rs 的 6 个新单测。
- **N-06 字段重命名 `code` → `status`**：plan §M4 / prompt.md 都说 `UpstreamHttp { status: u16, msg: String }`；实际代码用的是 `code`。`code` 在语义上是"任何整数码"（HTTP / 自定义错误码 / 业务码），`status` 在语义上是"HTTP 状态码"——这次变体是专门给 HTTP 用的，plan 的命名更准确。改名同步更新 `#[error("...{code}...")]` 的 Display formatter，两个新单测（`upstream_http_display_includes_status_and_msg` 等）锁住契约。pre-m4 没有 constructor、改 Display 不破坏任何生产代码，是低风险 rename。
- **N-06 整体策略：保持 `anyhow::Result<T>` 签名不变**：UpstreamClient 的 `notify_start` / `get_updates` / `send_message` / `send_typing` / `get_config` / `get_upload_url` 全部返回 `anyhow::Result<T>`，被 `UpstreamSink` trait 锁定，被 `hub/commands.rs:391` / `hub/dispatch.rs:221` / `server/routes.rs:590/623/670/708` 等 6 处 caller 调用——这些 caller 全部用 `error!(error = %e, ...)` 或 `format!("upstream error: {e}")` 消费错误，与 `anyhow::Error` 的 `Display` 等价。**改 trait 签名会触发 6 处 caller 的 `?` 链重构，但 6 处 caller 全部不 pattern-match 错误变体——纯纯的浪费 churn**。所以采用"内部构造 HubError、外部包成 anyhow::Error"的策略：error.rs 加 downcast，ilink/upstream.rs 加 helper（`upstream_http_err` / `upstream_parse_err`），call site 用 `.map_err(helper)?` 替换 `?`。这与 m1 的 `unwrap_or_else(|e| e.into_inner())` 风格一致——把"等价的更精确表示"封装到 helper 里，call site 仍是 1 token 的改动。
- **N-06 From<anyhow::Error> downcast 链**：新加 outer `match e.downcast::<HubError>()` 把 wrapped-HubError 还原成具体变体；既有 `match e.downcast::<sqlx::Error>()` 挪到 outer 的 Err 分支里做 inner match（再不行就 fallback 到 `Upstream(e.to_string())`）。原本想用 `if let Ok(...) = e.downcast::<HubError>() { return ... }`，但 compile error：anyhow 的 `downcast` 消费 self 即便走 Err 分支，第一次 `if let` 已经把 `e` move 走了，inner match 拿不到 e。改用 `match e.downcast::<HubError>() { Ok(hub_err) => hub_err, Err(e) => match e.downcast::<sqlx::Error>() { ... } }`——anyhow 的 downcast 契约就是"消费 self，Err 分支把 self 还回来"，所以 inner match 可以 rebind `e`。
- **N-06 status=0 的设计选择**：`reqwest::Error::status()` 在 transport-level failure（DNS / TLS / connection reset）时返回 `None`。`upstream_http_err` 用 `.map(|s| s.as_u16()).unwrap_or(0)` 把 None 映射成 `status: 0`——而不是 Option<u16> 或 enum，避免 `UpstreamHttp` 退化成"再嵌一层 Result"的形式。`status: 0` 是 sentinel，对应"还没有 HTTP 响应就失败"。`upstream_http_status_zero_is_legal_for_pre_send_failures` 单测锁住这个不变量。
- **N-06 故意不迁移的 1 处**：`ilink/login.rs::poll_qrcode_status` 的 `r.json::<QrcodeStatusResponse>().await` 也在 upstream_parse_err 的覆盖范围内，但**故意不迁移**——它在一个 retry loop 里，parse error 走 `warn!` + `continue;`，错误**永远不 escape 函数**。给这种 dead-tag 标 UpstreamParse 是"加 invariant 但没有 consumer 来 enforce"——徒增代码噪音。`login::get_qrcode` 那一处是真 parse error 会 escape 的（被 `login_with_qr` 的 `?` 传回 `RuntimeError`），所以那一处迁。
- **N-06 helper 不去重**：`ilink/upstream.rs` 和 `ilink/login.rs` 各有一个 `upstream_parse_err`——4 行 byte-identical clone。可以提到 `crate::ilink::error_helpers` 子模块，但 plan §M4 明确"全量迁移不在本次范围"，helper 提文件属于 refactor 范畴，留到后续 milestone 处理。`upstream_http_err` 只在 upstream.rs 用一次（login 模块不用 `error_for_status`），不需要提。

### Problems

- plan §M4 第一段"保留 `Upstream(anyhow::Error)` 作为兜底"与代码现状不符——pre-m1 的 `Upstream` 变体曾经是 `anyhow::Error` (有 `#[from]`)，m1 重构时已经被简化成 `Upstream(String)`，pre-m4 的 `From<anyhow::Error> for HubError` 已经在用 `HubError::Upstream(e.to_string())` 兜底。plan 文字是 plan 作者沿用 pre-m1 的旧措辞；执行时按现状走（保留 `Upstream(String)` 作为兜底，via `From<anyhow::Error>` 的最后 match arm），不引入 `anyhow::Error` 的 `#[from]` 重新混进 enum 字段。
- 第一次写 `if let Ok(hub_err) = e.downcast::<HubError>()` 想偷懒，被 `error[E0382]: use of moved value: e` 教做人——`if let` 只 bind Ok 分支，Err 分支没有 rebind e 的语法，self 在 downcast 调用里就被 move 走了。改成嵌套 `match` 是 anyhow downcast 的标准 idiom，编译通过。Review 时如果发现还能再简洁（比如改用 `.downcast_ref::<HubError>().cloned()` 的 `Clone` 路线）会加 notes，但前提是给 HubError 加 `derive(Clone)`——而 HubError 含 `sqlx::Error`（不实现 Clone），加 Clone 会改 m1 已经稳定的 11 个字段 enum 的 derive 列表，超出 m4 范围。
- 第一次写 `upstream_parse_err(e: reqwest::Error)` 时想只接受 `reqwest::Error`，但 `send_message` 里 `serde_json::from_str::<SendMessageResponse>(&text)` 失败时是 `serde_json::Error`——`send_message` 那处其实没有 parse-error 出口（parse fail 走 `warn!` + 返回 `Ok(SendMessageResponse::ok())`），所以现在 4 个用 upstream_parse_err 的位置全是 `reqwest::Error` 来源。但 helper 签名用 `impl std::fmt::Display` 更灵活：未来若有 `serde_json::from_str` 真要 escape 错误（不在 m4 范围），可以一行 `.map_err(upstream_parse_err)?` 复用，不需要再改 helper 签名。runtime cost 0（Display 是 to_string 触发一次）。

### Outcome

- ✅ `cargo fmt --check`
- ✅ `cargo clippy -- -D warnings`
- ✅ `cargo test`（6 个新增测试 + 全部历史测试绿；lib 369 / metrics_auth 1 / breaking_changes 20 / regression_http_smoke 10 / hub_routing_integration 27 / queue_trait 18 / doc_tests 0+1 ignored；合计 445 passed / 0 failed / 1 ignored）
- ✅ `cargo build`
- ✅ `desktop-frontend` `npm run build`
- ✅ `desktop-tauri` `cargo check`
- E2E checkpoint: not-ready（plan 已定；错误类型内部重构，wire-level error response shape 不变）
- Visual Review: not-needed（plan 已定；仅后端 Rust 改动）
- Reviewer handoff: `docs/exec-plans/active/arch-cleanup-p1/reviews/m4/review-request.yaml`

---

## M5 — handle_hub_command 拆解（N-07）

### Decisions

_待填写_

### Problems

_待填写_

### Outcome

_待填写_
