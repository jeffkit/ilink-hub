# Feature: prometheus-metrics-type-safe

## 目标

用 `prometheus` crate 替代 `src/server/routes.rs` 中手写 `format!` 拼接 Prometheus 文本格式的实现，消除因 label value 含特殊字符导致的非法 metrics 输出风险，并提升可维护性。

## 完成标准

- [ ] 引入 `prometheus = "0.13"` 依赖（或更新版本）
- [ ] 所有现有指标（clients_online, clients_total, messages_dispatched 等）通过 prometheus crate 注册和输出
- [ ] `/metrics` 端点输出格式与标准 Prometheus exposition format 兼容
- [ ] label value 含 `{`, `}`, `\n` 等特殊字符时不产生非法输出
- [ ] `cargo test` 全部通过
- [ ] `cargo clippy -- -D warnings` 零警告
- [ ] `cargo build` 成功

## 非目标

- 不改变 `/metrics` 端点路径
- 不引入 push-gateway 或其他 Prometheus 集成方式
- 不修改鉴权逻辑

## 背景 / 约束

- 文件：`src/server/routes.rs` 约第 926-1077 行手写 `# HELP / # TYPE` 和指标行
- `HubState::metrics` 已有 `AtomicU64` 字段，可直接读取值
- 注意 Hub 多实例部署时 Registry 不宜使用全局 static（可用 per-request collect 模式）
