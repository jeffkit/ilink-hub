# Plan: prometheus-metrics-type-safe

用 `prometheus` crate 替代 `src/server/routes.rs` 手写 Prometheus 文本格式，消除非法输出风险。

---

## 里程碑

### M1: 引入 prometheus crate 依赖

- 在 `Cargo.toml` 添加 `prometheus = { version = "0.13", default-features = false }`

```
cargo check 2>&1 | tail -5
```

### M2: 注册所有指标

- 新建 `src/metrics.rs`，用 `register_int_counter_vec!` / `register_int_gauge_vec!` 注册：
  - `ilink_hub_clients_online` (gauge, label: `hub`)
  - `ilink_hub_clients_total` (counter, label: `hub`)
  - `ilink_hub_messages_dispatched_total` (counter, labels: `hub, cmd`)
  - `ilink_hub_messages_dropped_total` (counter)
  - `ilink_hub_upstream_user_messages_total` (counter)
  - `ilink_hub_upstream_polls_ok_total` (counter)
  - `ilink_hub_upstream_polls_err_total` (counter)
  - `ilink_hub_sendmessage_total` (counter)
  - `ilink_hub_sendmessage_errors_total` (counter)
  - `ilink_hub_dispatcher_lagged_total` (counter)
  - `ilink_hub_relogin_attempts_total` (counter)
  - `ilink_hub_ilink_status` (gauge)
  - `ilink_hub_ctx_map_size` (gauge)
  - `ilink_hub_queue_size` (gauge, label: `client`)
  - `ilink_hub_persist_fire_and_forget_failures_total` (counter, label: `path`)
- Per-request `Registry`（非全局 static），从 `HubState::metrics` 的 `AtomicU64` 读取值并 set

```
cargo check 2>&1 | tail -5
```

### M3: 重构 `/metrics` 端点

- 替换 `routes.rs` 中 `format!` 拼接逻辑（约 926-1077 行），改为 `prometheus::TextEncoder::encode`
- 保留路由路径和鉴权逻辑不变

```
cargo build && curl -s http://localhost:3000/metrics | head -30
```

### M4: 特殊字符安全验证

- 构造含 `{`, `}`, `\n`, `\`, `"` 的 label value，确认输出合法

```
# 预期：所有行以 [a-zA-Z_:] 开头，无裸特殊字符
curl -s http://localhost:3000/metrics | grep -v '^#' | head -20
```

### M5: 全量检查

```
cargo test 2>&1 | tail -10
cargo clippy --all-targets -- -D warnings 2>&1 | tail -5
cargo build 2>&1 | tail -3
```

---

## E2E Checkpoint

> 每个里程碑完成后，将 `[ ]` 改为 `[x]` 并填写。

- [ ] **M1** — 日期: ___ | 耗时: ___ | 备注: ___
- [x] **M2** — 日期: 2026-06-17 | 耗时: ~20min | 备注: 所有验证通过
- [x] **M3** — 日期: 2026-06-17 | 耗时: ~5min | 备注: 核心重构在 M2 中已完成，M3 验证全部通过
- [x] **M4** — 日期: 2026-06-17 | 耗时: ~10min | 备注: 所有验证通过
- [x] **M5** — 日期: 2026-06-17 | 耗时: ~3min | 备注: 全部 6 项验证通过
