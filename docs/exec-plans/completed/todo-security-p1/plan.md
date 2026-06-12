# P1 Security Fixes — Execution Plan

> 修复 SEC-001（pair_confirm TOCTOU）、SEC-003（getupdates 并发 DoS）、SEC-013（pair_confirm 无认证）。
> 范围严格限定在三处文件，不涉及其他模块重构或依赖升级。

---

## 里程碑总览

| # | 里程碑 | 关联条目 | E2E Checkpoint |
|---|--------|----------|----------------|
| M0 | 基线（修复前确认可构建 / 测试） | — | ✅ |
| M1 | SEC-001 修复：pair_confirm 原子化 | SEC-001 | ✅ |
| M2 | SEC-003 修复：getupdates 并发上限 + 429 | SEC-003 | ✅ |
| M3 | SEC-013 修复：pair_confirm 认证三件套 | SEC-013 | ✅ |
| M4 | 质量门禁收口 | — | ✅ |

---

## M0 — 基线确认  `[Checkpoint]`

**目标**：确保工作树干净，修复前的测试/编译是绿的，避免把"原本就坏"的污染带进验证。

**步骤**：
1. `git status` 确认在 todo-security-p1 worktree 干净状态。
2. 记录基线 clippy 输出。

**验证命令**：
```bash
git status
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**通过条件**：
- 上一轮 `git status` 只显示 `.flowx/`、`docs/exec-plans/active/todo-security-p1/`（已存在），无未提交源码改动。
- 编译成功，clippy 无 warning，所有测试通过。

---

## M1 — SEC-001：pair_confirm 原子化（写锁内 register → confirm）  `[Checkpoint]`

**问题**：`src/server/pairing.rs:381-386` 中 `register_client_in_hub` 与 `pairing.write().await` + `confirm` 跨两个独立的锁边界，TOCTOU 窗口可被并发 confirm 抢断，导致首个请求占用 vtoken、合法操作员的客户端拿到 409。

**目标**：在 `state.pairing.write()` 持锁期间原子完成「校验状态 → register_client → confirm」，中间状态对外不可见。

**改动文件**：
- `src/server/pairing.rs`（`pair_confirm` handler）
- `src/hub/pairing.rs`（`PairingRegistry::confirm` 签名调整：可选接受 `&mut ClientRegistry` / 或在锁内回调注册逻辑；优先方案 = 在 `pair_confirm` 拿 pairing 写锁后**先调** `register_client_in_hub` 并把生成 vtoken 通过 `confirm` 提交；锁内三步全闭环）

**实施要点**：
1. 重构 `pair_confirm`：先取 `state.pairing.write()`；在锁内做：
   - `purge_expired` + 状态校验（NotFound / Expired / AlreadyConfirmed 提前返回）。
   - 生成 vtoken（调用 `register_client_in_hub`，或把 vtoken 生成抽成纯函数以便在锁内调用）。
   - 调 `pairing.confirm(...)` 落 vtoken + 状态。
2. 锁释放后再做 `info!` 等观察性日志（避免长持锁）。
3. 不动 `PairingRegistry::confirm` 现有签名（如需校验更严格的状态——例如 SEC-013 里的 `Scanned`——放到 M3 一起做）。

**验证命令**：
```bash
cargo test -p ilink-hub --lib hub::pairing
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

**新增测试**（`src/hub/pairing.rs` 单元测试 + `tests/hub_routing_integration.rs` 集成测试）：
- `tests/hub_routing_integration.rs` 增补：并发对同一 `code` 发 N 个 `POST /hub/pair/{code}/confirm`，断言：
  - 恰好 1 个返回 200 并带 vtoken；
  - 其余返回 409 `pairing already confirmed`；
  - `ClientRegistry` 中该 name 仅有 1 条记录，无重复/被抢断的 vtoken。
- `src/hub/pairing.rs`：单元测试 `confirm_after_concurrent_attempt_returns_only_one_winner`（调用 `PairingRegistry` 串行模拟两个 `confirm` 调用，验证后者得 `AlreadyConfirmed`）。

**通过条件**：
- 上述新测试通过。
- 现有 `pairing` 测试与 `hub_routing_integration` 全绿。
- clippy 无 warning。

---

## M2 — SEC-003：getupdates 并发上限 + 429  `[Checkpoint]`

**问题**：`src/server/routes.rs:141-226` 中 `PollTracker::enter` 只打 warn 不阻断，单 vtoken 可耗尽 worker 线程。

**目标**：在 `getupdates` handler 中，超过每 vtoken 并发阈值（如 3）时立即返回 HTTP 429（且**不**进入 `wait_notify_or_shutdown`）。

**改动文件**：
- `src/server/routes.rs`（`getupdates` handler 入口处的并发闸门）
- `src/hub/mod.rs`（新增 `MAX_CONCURRENT_POLLS_PER_VTOKEN` 常量；如 `PollTracker::enter` 当前签名不足以区分"探测性 enter"与"真正占用"——确认现有 `enter` 已经返回 RAII guard，429 分支必须**丢弃** guard 才能避免计数泄漏）

**实施要点**：
1. 在 `src/hub/mod.rs` 顶部加 `pub const MAX_CONCURRENT_POLLS_PER_VTOKEN: usize = 3;`。
2. 在 `routes.rs::getupdates` 中、注册 `mark_seen` **之前**先调 `state.poll_tracker.enter(&vtoken)`：
   - 若 `count > MAX_CONCURRENT_POLLS_PER_VTOKEN`：丢弃 guard（让其 `Drop` 自减——会回到合法值）、返回 `StatusCode::TOO_MANY_REQUESTS` + JSON `{ "ret": 429, "errmsg": "too many concurrent polls for this vtoken" }`；**不要**进 `wait_notify_or_shutdown`，避免持有额外资源。
   - 否则正常 `mark_seen` + 走原流程。
3. 注意：原本 `enter` 是 `mark_seen` 之后才调用（M1 后逻辑顺序需要 review）；SEC-003 修复要求"先 enter、超阈值直接拒"——这一调整与 SEC-001 互不冲突，但要让 warn 日志（>1 时的 split-brain 告警）继续在通过闸门后保留。
4. 把 `>1` 那个 split-brain warn 改成 `>1 && <= MAX`：超过 MAX 已经被 429 拦下，再触发 warn 没意义。

**验证命令**：
```bash
cargo test -p ilink-hub --lib
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

**新增测试**：
- `src/hub/mod.rs` 单测 `poll_tracker_caps_concurrent`：连续 `enter` 4 次，断言第 4 次 `count == 4 > 3`。
- `tests/hub_routing_integration.rs` 集成测试 `getupdates_returns_429_when_polls_exceed_cap`：
  - 注册一个 vtoken；
  - 用 `tokio::join!` 启动 4 个 `getupdates` 长轮询；
  - 第 4 个请求在合理超时（< 2s）内收到 429；
  - 释放 1 个长轮询（drop guard）后，再发一个 getupdates 应当 200。

**通过条件**：
- 新测试通过。
- 既有的 split-brain warn 行为不退化（并发 2 的场景仍会 warn）。
- clippy 无 warning。

---

## M3 — SEC-013：pair_confirm 认证三件套  `[Checkpoint]`

**问题**：`POST /hub/pair/{code}/confirm` 无认证 + `pair_url` 走 INFO 日志 → 任何能读 Hub 日志的人可注册任意 vtoken。

**目标**：
1. 日志降级：`pair_url` 改 DEBUG。
2. 状态门：confirm 仅在 `PairingStatus::Scanned` 时通过（手机扫码是唯一合法触发）。
3. CSRF：`pair` HTML 页面生成一次性 token（绑定 code），confirm 请求必须携带并校验。

**改动文件**：
- `src/server/pairing.rs`（日志降级、`pair_page` 注入 CSRF token、`pair_confirm` 校验 CSRF + 状态门）
- `src/server/pair.html`（页面 fetch 时附带 `X-Pair-CSRF` header）
- `src/hub/pairing.rs`（`PairingRegistry` 中 `confirm` 增加 `Scanned` 状态前置检查，返回新 `PairingError::NotScanned` 变体；为 CSRF token 留存储位——可在 `PairingSession` 加 `csrf: Option<String>`，由 `mark_scanned` 同时生成；或在 `pair_page` handler 中用独立 map——优先用 `PairingSession` 内字段，避免新结构）

**实施要点**：

### 3.1 日志降级
- `src/server/pairing.rs:221` `info!(code = %code, pair_url = %pair_url, ...)` → `debug!(code = %code, pair_url = %pair_url, ...)`。
- 同时审阅 209 / 253 / 304 / 390 行 info! 是否泄漏敏感字段，统一降到 debug 或脱敏（pair_url 含未确认的活跃 code，足够敏感）。

### 3.2 状态门（`Scanned` 前置）
- `PairingRegistry::confirm` 开头增：
  ```rust
  if session.status != PairingStatus::Scanned {
      return Err(PairingError::NotScanned);
  }
  ```
  （注意 Expired 已在 is_expired 分支先处理；这里只挡 Wait / Confirmed / 已 Expired 的边缘场景。）
- 新增 `PairingError::NotScanned` 变体，`HTTP` 映射到 412 Precondition Failed（或 403 — 选 412 表示"前置条件不满足：未扫码"）。
- `tests/hub_routing_integration.rs` 新增 `pair_confirm_rejected_when_not_scanned`：创建 pairing 后**不**调用 `mark_scanned`，直接 confirm，期望 412。

### 3.3 CSRF token
- `PairingSession` 新增 `csrf: Option<String>`（`#[serde(skip)]` 字段不可序列化）。
- `mark_scanned(&mut self, code: &str)` 内同时 `session.csrf = Some(generate_csrf())`（`generate_csrf` 用 `rand::Rng` 16 字节 hex，32 字符；检查项目是否已有 `rand` crate，无则用 `getrandom`——查 `Cargo.toml`）。
- `pair_page` handler：调 `mark_scanned` 之后从 session 读 csrf，注入到 `PAIR_HTML_TEMPLATE` 的 `__PAIR_CSRF__` 占位符（与 `__PAIR_CODE__` 同样的 `replace` 模式）。
- `pair_confirm` handler：
  - 接收 `HeaderMap`，要求存在 `X-Pair-CSRF` header（与 body 中 name 解耦，浏览器 fetch 走 header 更直接）。
  - 在 pairing 写锁内**先**比较 `session.csrf.as_deref() == Some(header_value)`，不匹配返回 403 `csrf_mismatch`。
  - 校验通过后**消费** token（`session.csrf = None`），防止重放。
  - 顺序：CSRF → Scanned 状态 → register → confirm。
- `src/server/pair.html` 中 `fetch("/hub/pair/{code}/confirm", { method: "POST", headers: { "Content-Type": "application/json", "X-Pair-CSRF": "__PAIR_CSRF__" }, body: JSON.stringify({name}) })`。
- 模板中"已配对" / "已过期" 分支不再渲染 csrf 字段（避免泄露到客户端的最终成功页）。

**验证命令**：
```bash
cargo test -p ilink-hub --lib hub::pairing
cargo test -p ilink-hub --lib server::pairing
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

**新增测试**：
- `src/hub/pairing.rs`：
  - `confirm_rejected_when_status_is_wait`（不调 mark_scanned → NotScanned）。
  - `csrf_token_consumed_after_confirm`（确认后 `session.csrf` 为 None）。
- `tests/hub_routing_integration.rs`：
  - `pair_confirm_requires_valid_csrf_header`：带错/缺 `X-Pair-CSRF` → 403。
  - `pair_confirm_succeeds_with_correct_csrf_and_scanned_state`：完整链路（mark_scanned → 取 csrf → confirm）→ 200。
  - `pair_confirm_csrf_cannot_be_reused`：成功 confirm 后再带同一 csrf 重发 → 403。
  - `pair_confirm_rejected_when_not_scanned`（已在 3.2 列出）。

**通过条件**：
- 所有新测试通过。
- 既有的 `pairing` / `hub_routing_integration` 测试全绿。
- 抓包检查：INFO 级别日志中不再含 `pair_url=`（DEBUG 保留以供排查）。
- clippy 无 warning。

---

## M4 — 质量门禁收口  `[Final Checkpoint]`

**验证命令**：
```bash
cargo build --workspace --release
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo fmt --check
git diff --stat
```

**通过条件**（对齐 prompt.md 完成标准）：
- [ ] SEC-001 修复已提交（commit 1）
- [ ] SEC-003 修复已提交（commit 2）
- [ ] SEC-013 修复已提交（commit 3，可拆多个小 commit 但同 PR）
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 无新 warning
- [ ] `cargo test --workspace` 全绿
- [ ] `cargo fmt --check` 通过
- [ ] PR 描述列出三处 CWE、修复机制、新增测试

**建议 commit 拆分**（便于 review 与回滚）：
1. `fix(pairing): atomically register+confirm to close TOCTOU (SEC-001)`
2. `fix(routes): cap concurrent getupdates per vtoken (SEC-003)`
3. `fix(pairing): require Scanned state + CSRF; redact pair_url from info logs (SEC-013)`
4. `chore: green the quality-gate baseline`（若 clippy/fmt 需要微调，独立一笔）

**非目标重申**（来自 prompt.md）：
- 不重构不涉及上述条目的其他模块。
- 不升级无关依赖（如确需新增 `rand` / `getrandom`，仅在 SEC-013 引入 CSRF 的最小范围内）。

---

## E2E Checkpoint 汇总

| 阶段 | Checkpoint | 触发条件 | 通过判定 |
|------|-----------|----------|----------|
| M0 | 基线 | 进入实施前 | 编译/clippy/测试全绿 |
| M1 | SEC-001 完成 | 锁内三步原子化 + 并发 confirm 测试 | 新测试通过，回归通过 |
| M2 | SEC-003 完成 | 429 闸门 + 超阈值拒绝测试 | 闸门在 guard drop 后恢复，新测试通过 |
| M3 | SEC-013 完成 | 日志降级 + Scanned 状态门 + CSRF 三件套 | 4 项新测试通过，INFO 日志无 pair_url |
| M4 | 收口 | PR 提交前 | 全部 prompt.md 完成标准勾选 |

---

## 风险与回滚

- **M1 风险**：把 `register_client_in_hub` 移进 `pairing.write()` 持锁区间，会延长写锁持有时间（包含一次异步 `ClientRegistry` 写）。**缓解**：先在 `Cargo.toml` 查 `register_client_in_hub` 是否需独立写锁——若是，把"vtoken 生成"抽成纯函数（`generate_vtoken()`），写锁内只做纯计算 + 状态翻转；ClientRegistry 的插入放到锁外（在 confirm 返回 vtoken 字符串之后异步 `registry.register`）。回滚：单 commit revert 即可。
- **M2 风险**：429 分支丢弃 PollGuard 后才 `Drop`，计数自减时机与原始"通过闸门 → 走完 handler"路径一致，无泄漏。**回滚**：保留 `>1` warn 逻辑，去掉 429 分支即可恢复旧行为。
- **M3 风险**：CSRF token 存储在 `PairingSession` 中，会随 session 过期被 purge；**用户刷新 pair 页面**会触发 `mark_scanned` 重入并刷新 csrf——当前 `mark_scanned` 实现已允许 Scanned → Scanned 安全重入（`src/hub/pairing.rs:94-96`），csrf 字段同时覆盖即可。**回滚**：删除 `csrf` 字段 + 移除校验即可。
