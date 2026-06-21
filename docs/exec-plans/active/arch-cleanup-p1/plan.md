# arch-cleanup-p1 — Plan

## 范围

单仓 ilink-hub，纯 Rust 改动，不涉及 HTTP API schema 变更。改动范围：
`src/hub/state.rs`、`src/hub/health.rs`（或 registry 注销路径）、`src/bridge/dispatcher.rs`、
`src/store/migrations.rs`、`src/hub/mod.rs`（命令拆分）、`src/error.rs`、`Cargo.toml`。

## 架构关系

```
┌─────────────────────────────────────────────────────────┐
│ M1: 可靠性修复 (N-01, N-04)                              │
│   last_seen cleanup + Mutex poison safety                │
│   影响：hub/state.rs、bridge/dispatcher.rs               │
├─────────────────────────────────────────────────────────┤
│ M2: 可观测性精度 (N-02)                                  │
│   LatencyHistogram sum_us 微秒精度                        │
│   影响：hub/state.rs、server/routes.rs                   │
├─────────────────────────────────────────────────────────┤
│ M3: 低风险修复 + 依赖升级 (N-03, N-05)                   │
│   column_exists MySQL 占位符 + rand 0.9 升级             │
│   影响：store/migrations.rs、Cargo.toml + 调用点         │
├─────────────────────────────────────────────────────────┤
│ M4: HubError 具体化 (N-06)                              │
│   新增 UpstreamHttp / UpstreamParse 变体                │
│   影响：error.rs + 上游调用链                            │
├─────────────────────────────────────────────────────────┤
│ M5: handle_hub_command 拆解 (N-07)                      │
│   命名函数提取，单测覆盖                                  │
│   影响：hub/mod.rs 或 hub/commands.rs                    │
└─────────────────────────────────────────────────────────┘
```

---

## M1 — 可靠性修复（N-01 + N-04）

**目标**：消除 `last_seen` 内存泄漏隐患，统一 Mutex poison 防御风格。

**改动点**：
- N-01：在客户端注销路径（`remove_client` / `unregister`，通常在 `hub/health.rs` 的 offline-cleanup 或 `server/routes.rs` 的 admin-delete handler）同步调用 `state.clients.last_seen.remove(&vtoken_hash)`。
- N-04：`src/bridge/dispatcher.rs` 中 `SessionDispatcher::dispatch()` 的 `self.senders.lock().expect(...)` 改为 `self.senders.lock().unwrap_or_else(|e| e.into_inner())`，与已有的 `evict_closed_senders` 保持一致。

**验证命令**：
```bash
cargo test -p ilink-hub --lib hub::tests -- --nocapture 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | tail -5
```

**E2E checkpoint：** not-ready  
**E2E 判定依据：** e2e-protocol Step B：内部模块改动（内存结构清理 + 锁 API），无新的外部 HTTP endpoint → `not-ready`  
**Visual Review：** not-needed

---

## M2 — 可观测性精度（N-02）

**目标**：让 `LatencyHistogram._sum` 在亚毫秒场景下不全为 0，提升 Grafana avg 可信度。

**改动点**：
- `src/hub/state.rs`：`LatencyHistogram` 新增 `sum_us: AtomicU64`，`observe()` 参数改为接受 `Duration`（内部转 μs 存入 `sum_us`，同时保留原 `sum_ms` 或直接废弃 → 用 `sum_us / 1000` 替代）。
- 调用 `observe()` 的地方（`routes.rs` 的 `HistoGuard` / `LatencyGuard`）改为传 `Duration`。
- `src/server/routes.rs` 的 `render_histogram`：输出 `_sum` 时用 `sum_us / 1000`（毫秒，符合 Prometheus 惯例）。
- 新增单测：观测 0 个毫秒（如 500μs）后验证 `sum_us > 0`。

**验证命令**：
```bash
cargo test -p ilink-hub --lib hub::state 2>&1 | tail -10
cargo test -p ilink-hub -- latency 2>&1 | tail -10
cargo clippy -- -D warnings 2>&1 | tail -5
```

**E2E checkpoint：** not-ready  
**E2E 判定依据：** e2e-protocol Step B：`/metrics` 输出格式变更（内部精度），不需要 HTTP 端到端验证（单测已覆盖）→ `not-ready`  
**Visual Review：** not-needed

---

## M3 — 低风险修复 + rand 升级（N-03 + N-05）

**目标**：修复潜在的 MySQL 占位符 bug，升级 `rand` 至 0.9。

**改动点**：
- N-03：`src/store/migrations.rs` 的 `column_exists()` 函数，在 `DatabaseKind::MySql` 分支使用 `?` 占位符（`WHERE table_name = ? AND column_name = ?`），并移除 `#[allow(dead_code)]` 标注（确保 clippy 持续检查）。
- N-05：`Cargo.toml` 中 `rand` 升至 `"0.9"`，`rand_core` 同步升级；搜索项目内 `rand::thread_rng()` 等弃用 API，按 0.9 迁移指南更新（主要是 `rand::random::<T>()` 替代 `thread_rng().gen()`）。

**验证命令**：
```bash
cargo update
cargo test 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | tail -5
cargo fmt --check 2>&1
```

**E2E checkpoint：** not-ready  
**E2E 判定依据：** e2e-protocol Step B：依赖升级 + dead-code 修复，无新外部接口 → `not-ready`  
**Visual Review：** not-needed

---

## M4 — HubError 具体化（N-06）

**目标**：新增 `UpstreamHttp` 和 `UpstreamParse` 变体，让调用方可区分错误类型，保留 `Upstream(anyhow::Error)` 作为兜底。

**改动点**：
- `src/error.rs`：新增以下变体：
  ```rust
  #[error("upstream HTTP error: status={status}, msg={msg}")]
  UpstreamHttp { status: u16, msg: String },
  #[error("upstream response parse error: {0}")]
  UpstreamParse(String),
  ```
- 搜索当前 `HubError::Upstream(anyhow::anyhow!(...))` 的使用点，将 HTTP 错误（含 status code 的）改为 `UpstreamHttp`，将 JSON 解析错误改为 `UpstreamParse`；其余保持 `Upstream`。
- 目标：至少 3 处调用点迁移，形成示范；全量迁移不在本次范围。

**验证命令**：
```bash
cargo test 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | tail -5
grep -r "HubError::Upstream(" src/ | grep -v "//.*HubError" | wc -l  # 应有减少
grep -r "UpstreamHttp\|UpstreamParse" src/ | wc -l                    # 应 ≥ 3
```

**E2E checkpoint：** not-ready  
**E2E 判定依据：** e2e-protocol Step B：错误类型内部重构，调用方行为等价 → `not-ready`  
**Visual Review：** not-needed

---

## M5 — handle_hub_command 拆解（N-07）

**目标**：`handle_hub_command` match 退化为纯分发，每个 `HubCommand` 变体提取为具名 async fn，可独立单测。

**改动点**：
- 目标文件：`src/hub/commands.rs`（或 `src/hub/mod.rs`，视当前实际位置）。
- 枚举拆分策略：按功能域提取，如 `handle_list()`、`handle_use()`、`handle_session_new()`、`handle_session_list()` 等。
- 每个提取函数接受 `&Arc<HubState>` + 必要参数，返回 `Result<String>` 或等价类型。
- 新增单测：至少覆盖 `handle_list`、`handle_use`、`handle_session_new` 三个路径（mock HubState 或用 in-memory store）。
- match 本体每个分支 ≤ 3 行（函数调用 + 返回）。

**验证命令**：
```bash
cargo test -p ilink-hub -- hub::commands 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | tail -5
# 验证 match 没有超长分支（每分支 ≤ 3 行，人工检查）
wc -l src/hub/commands.rs 2>/dev/null || wc -l src/hub/mod.rs
```

**E2E checkpoint：** yes  
**E2E 判定依据：** e2e-protocol Step B：`handle_hub_command` 是 Hub 命令处理核心路径，重构后需通过 cargo test 的集成测试确认行为等价 → `yes`（最后一个里程碑必须为 yes）  
**E2E 场景：** `cargo test` 全绿，确认现有 hub 命令路由测试（`/list`、`/use`、`@name` 等）全部通过  
**Visual Review：** not-needed

---

## 全局验证命令

```bash
# 最终质量门（每个里程碑结束后也运行）
cargo fmt --check && cargo clippy -- -D warnings && cargo test
```

## 风险

- **N-05 rand 0.9**：rand 0.9 与 0.8 有 API break（`thread_rng()` 已弃用），需要搜索全部使用点。用 `cargo test` 验证，若有隐藏依赖通过 `cargo tree` 定位。
- **N-07 拆解**：`handle_hub_command` 中部分分支可能共用 helper 变量（如 `resolve_vctx`），提取时需注意借用生命周期，避免 `Arc::clone` 过度传递。
- **N-06**：`HubError::Upstream` 有 `#[from] anyhow::Error` 特性，新增变体后不影响 `?` 传播，但需确认 `thiserror` 生成代码无冲突。
