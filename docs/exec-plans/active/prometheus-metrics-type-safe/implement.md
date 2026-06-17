# Implement: prometheus-metrics-type-safe

## 完成状态

- [x] **M1** — 日期: 2026-06-17 | 耗时: ~2min | 备注: 所有验证通过（1 个预存 flaky test 除外）
- [x] **M2** — 日期: 2026-06-17 | 耗时: ~20min | 备注: 所有验证通过，6 个单元测试覆盖
- [x] **M3** — 日期: 2026-06-17 | 耗时: ~5min | 备注: 核心重构在 M2 中已完成，M3 验证全部通过
- [x] **M4** — 日期: 2026-06-17 | 耗时: ~10min | 备注: 所有验证通过
- [x] **M5** — 日期: 2026-06-17 | 耗时: ~3min | 备注: 全部 6 项验证通过（fmt/clippy/test/build/desktop-frontend/desktop-tauri）

## M1 详情

在 `Cargo.toml` 添加 `prometheus = { version = "0.13", default-features = false }` 依赖。
Cargo.lock 自动更新。

## M2 详情

新建 `src/metrics.rs`，实现 `gather_metrics` 函数：
- 接受 `&HubState` + `hub_name: &str`，返回 `Result<String, prometheus::Error>`
- 创建 per-request `Registry`，用 `IntCounterVec` / `IntGaugeVec` 注册全部 15 个指标
- 从 `HubState::metrics` 的 `AtomicU64` 字段及各子状态读取值并 set/counter inc
- 用 `TextEncoder::encode_to_string` 编码输出
- 在 `src/lib.rs` 添加 `pub mod metrics`
- 6 个单元测试验证指标完整性、数值正确性、标签顺序、输出格式

## M3 详情

重构 `/metrics` 端点：
- `routes.rs` 中旧的 `format!` 拼接逻辑（原约 150 行）已在 M2 中一并移除
- `routes.rs:997` 直接调用 `crate::metrics::gather_metrics(&state, &hub_name).await`
- 路由路径 `/metrics` 和 `check_admin_auth` 鉴权逻辑保持不变
- 全部 6 项验证通过：fmt、clippy、test（327 passed）、build、desktop-frontend、desktop-tauri

## M4 详情

特殊字符安全验证：
- 新增 3 个测试覆盖 `\`、`{`、`}` 及组合特殊字符（`{`, `}`, `\`, `"`, `\n`）场景
- 结合已有测试，完整覆盖 plan.md 要求的全部特殊字符：`{`, `}`, `\n`, `\`, `"`
- 验证 prometheus crate 的 label 转义机制正确输出合法格式
- 全部 6 项验证通过：fmt、clippy、test（330 passed）、build、desktop-frontend、desktop-tauri
