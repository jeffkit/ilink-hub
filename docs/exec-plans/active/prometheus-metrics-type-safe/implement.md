# Implement: prometheus-metrics-type-safe

## 完成状态

- [x] **M1** — 日期: 2026-06-17 | 耗时: ~2min | 备注: 所有验证通过（1 个预存 flaky test 除外）
- [x] **M2** — 日期: 2026-06-17 | 耗时: ~20min | 备注: 所有验证通过，6 个单元测试覆盖
- [ ] **M3**
- [ ] **M4**
- [ ] **M5**

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
