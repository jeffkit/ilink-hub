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

_待填写_

### Problems

_待填写_

### Outcome

_待填写_

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
