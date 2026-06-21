# arch-cleanup-p1

## 目标

修复架构深度评审（2026-06-21）发现的 7 个残留问题，涵盖可靠性、可观测性精度、可维护性和依赖更新，让 ilink-hub 长期运行更健壮、指标更可信、代码更易维护。

## 背景

本次问题来源于对照 2026-06-10 / 2026-06-14 两份旧审查报告后的增量 review，所有旧 P0/P1 问题已修复，剩余 7 项均为 P2/P3 级，未影响当前生产，但若放置将随业务增长演变为更大风险。

## 完成标准

每项可通过命令或具体场景独立验证：

1. **N-01 `last_seen` 清理**：`ClientRegistry::remove` 路径执行后，`state.clients.last_seen` 中对应 vtoken hash 的条目不存在；新增单测验证注销 → `last_seen` 为空。
2. **N-02 延迟精度**：`LatencyHistogram` 改为微秒累加（`sum_us`），`/metrics` 端点输出 `_sum` 单位为毫秒（除以 1000）；对 0.5ms 操作的观测，`sum` 不再为 0。新增单测验证亚毫秒观测后 `sum_us` > 0。
3. **N-03 `column_exists()` MySQL 兼容**：`column_exists` 函数 MySQL 分支使用 `?` 占位符；`cargo clippy` 通过。
4. **N-04 Mutex poison 一致性**：`SessionDispatcher::dispatch()` 和 `evict_closed_senders()` 均使用 `unwrap_or_else(|e| e.into_inner())`，消除 `.expect()` panic 不一致。
5. **N-05 `rand` 升级**：`Cargo.toml` 中 `rand` 版本升级至 `0.9`，`rand_core` 对应升级，所有调用点更新；`cargo test` 全绿。
6. **N-06 `HubError` 具体变体**：新增 `HubError::UpstreamHttp { status: u16, msg: String }` 和 `HubError::UpstreamParse(String)` 替换 `Upstream(anyhow::Error)` 的部分用法；调用方可区分 HTTP 失败与解析失败；`cargo clippy -- -D warnings` 无 warning。
7. **N-07 `handle_hub_command` 拆解**：核心 match 体中每个 `HubCommand` 分支提取为具名 `async fn handle_cmd_XXX`，match 本体退化为纯分发（每分支 ≤ 3 行）；提取函数各有对应单测；`cargo test` 新增测试全绿。
8. **回归**：`cargo fmt --check && cargo clippy -- -D warnings && cargo test` 全部通过。

## 硬约束

- N-02：`/metrics` 端点的 `_sum` 字段**单位保持毫秒**（Prometheus 生态惯例），仅内部 `sum_us` 存微秒，输出时除以 1000。
- N-05：`rand` 升级后不引入任何新的 `unwrap()` 或弃用 API 使用；`rand_core` 同步升级保持 feature 兼容。
- N-06：`HubError::Upstream(anyhow::Error)` 变体**保留**作为兜底（不完全删除），只是新增更具体的变体供调用方使用；不破坏现有 `?` 传播链。
- N-07：提取后的函数签名必须接受 `&Arc<HubState>` 而非整个 state，缩小依赖范围；不改变任何 HTTP API 的 request/response schema。
- 所有改动必须通过 `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`。

## 非目标

- 不做 Redis 队列 backend 实现（已有计划）。
- 不做 `HubError` 的全量枚举重构（只新增具体变体，不删除 catch-all）。
- 不做 Postgres/MySQL 的 `column_exists()` 运行时测试（仅修复代码正确性）。
- 不做任何 HTTP API 的 breaking change。
- 不做 `rand` 0.9 之外的其他依赖升级（本次范围仅限 rand + rand_core）。
