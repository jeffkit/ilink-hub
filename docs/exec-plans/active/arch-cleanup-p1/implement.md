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

_待填写_

### Problems

_待填写_

### Outcome

_待填写_

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
