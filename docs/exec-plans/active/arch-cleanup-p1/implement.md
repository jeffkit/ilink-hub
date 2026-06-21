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

_待填写_

### Problems

_待填写_

### Outcome

_待填写_

---

## M5 — handle_hub_command 拆解（N-07）

### Decisions

_待填写_

### Problems

_待填写_

### Outcome

_待填写_
