# Plan: bridge-partial-throttle-retry

> 对应 prompt.md — 把 Bridge 对 `ret=-2` 的"丢弃"行为改为"缓冲 + 合并 + 指数退避重试"。
> 范围严格限定在本 PR 的"非目标"之外。

## 里程碑总览

| # | 里程碑 | 关键产物 | 验证命令 | E2E Checkpoint? |
|---|---|---|---|---|
| M0 | 现状基线 | 跑通既有 `cargo test` 与 lint，确认 main 干净 | `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test` | ✅ E2E-0 |
| M1 | `HubClient::sendmessage()` 返回类型化结果 | `dispatcher.rs` 引入 `SendOutcome { Sent, Throttled, Failed(...) }`，调用方用 `match` 区分；HTTP/JSON 结构零变化 | `cargo build && cargo clippy --all-targets -- -D warnings` | — |
| M2 | partial 分段转发改造：缓冲+合并+指数退避 | `partial_rx` 任务改为有状态循环：`pending: String` + 退避计时器（起步 5s，×2，封顶 60s）；`Sent` 清空+重置计时器；`Throttled` 不清空、退避后重试；`Failed` 维持原行为 | `cargo test dispatcher::tests` | ✅ E2E-1 |
| M3 | 最终回复路径同改：限流时不吞收尾 | ILINK_PARTIAL-only 导致 body 为空、转而持久化 `cli_session_id` 的分支、以及非空 body 的最终发送，同样走 `match SendOutcome` + 缓冲重试 | `cargo test` | ✅ E2E-2 |
| M4 | 放弃上限 + 清晰日志 | 引入最大累计重试时长（与 profile `timeout_secs` 关联），到点放弃并打 `error!` 日志（包含放弃的消息片段长度/耗时） | `cargo test` | ✅ E2E-3 |
| M5 | 新增 e2e/单测：mock 上游先返回 N 次 `-2` 再成功 | 在 `tests/e2e_wechat_simulation.rs`（或 bridge 单元测试）构造"前 N 次 `sendmessage` 返回 `{"ret":-2}`、之后成功"；断言：(a) 被限流内容**最终送达**；(b) 顺序保持；(c) 无丢失；(d) 退避节奏逐步拉长 | `cargo test --test e2e_wechat_simulation` | ✅ E2E-4 |
| M6 | 代码质量闸 | 格式化 + clippy 零告警 + 全量测试绿 | `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test` | ✅ E2E-5 |

## E2E Checkpoint 详情

### ✅ E2E-0 — 现状基线
**目的**：在动代码前确认基线干净，避免后续被既有失败污染结论。
**步骤**：
1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`
**通过条件**：三条命令全部 0 退出码；记录现有测试用例总数。
**备注**：若基线已红，先把基线问题隔离到独立 PR，不混入本 feature。

### ✅ E2E-1 — partial 缓冲+退避核心闭环
**触发条件**：M2 完成（partial 转发循环改造完）。
**验证项**：
- 在 `cargo test` 框架下用 mock `UpstreamSink`（`tests/e2e_wechat_simulation.rs` 已存在）模拟：
  - 场景 A：先返 3 次 `{"ret":-2}`，第 4 次成功 → 断言**整段被一次性送达**，且到达顺序 = 发送顺序。
  - 场景 B：连发 N 个 partial chunk，期间限流不退 → 断言退避间隔**单调不递减**，并稳定收敛到 ≤ 60s。
- 单元层面：抽取退避计算为纯函数（`fn backoff_for(attempt: u32) -> Duration`），断言序列 `[5, 10, 20, 40, 60, 60, ...]`。
**通过条件**：新增/扩展的测试全部通过；既有测试无回归。
**短路条件**：若 `partial_rx` 任务结构改造较大无法在单测触达，则退化为在 `dispatcher` 内部抽一个 trait-抽象的 sender，单测中注入。

### ✅ E2E-2 — 最终回复路径覆盖
**触发条件**：M3 完成。
**验证项**：
- 场景 C：部分 partial 触发过 `-2` 后成功，最终回复（空 body 走 `cli_session_id` 持久化分支）再遇 `-2` → 断言最终回复最终送达，会话被持久化。
- 场景 D：非空 body 最终发送遇 `-2` → 同上断言。
**通过条件**：场景 C/D 通过；场景 A/B 仍绿。

### ✅ E2E-3 — 放弃上限
**触发条件**：M4 完成。
**验证项**：
- 场景 E：mock 永久返 `-2`，验证在累计重试时长达到上限时**主动放弃**，日志包含可定位信息（放弃时的 `pending` 长度、累计耗时、最近一次错误）。
- 断言：放弃后 partial 循环**优雅退出**（不退化为进程 panic），后续 chunk 不再无谓堆积。
**通过条件**：场景 E 通过；放弃时长可配置（默认与 `timeout_secs` 挂钩，留有 override 接口）。

### ✅ E2E-4 — 完整 e2e 场景
**触发条件**：M5 完成。
**验证项**：在 `tests/e2e_wechat_simulation.rs` 中合入一个完整 e2e：
- 用真实 Bridge 启动一个 iLink 客户端；
- mock 上游先限流 N 次后恢复；
- 灌入一批 partial chunk（≥10 条）+ 最终回复；
- 断言：
  1. **所有消息最终送达**，无丢失；
  2. 送达顺序 = 发送顺序；
  3. 中间观察到的发送间隔**至少有一次** ≥ 5s（即起步退避生效）；
  4. 没有出现"高频盲发加剧限流"的反模式（连续 ≥3 次发送间隔 < 1s 视为失败）。

### ✅ E2E-5 — 合并质量闸
**触发条件**：M6 完成（全部里程碑合并后）。
**步骤**：
1. `cargo fmt --all`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`（全量）
**通过条件**：三条全 0；与 E2E-0 基线对比测试用例数增长 ≥ 3（新增场景至少 3 个）。

## 关键实现约束（承袭 prompt.md）

- **不**引入新协议字段 / 新 HTTP 端点；限流信号沿用 `SendMessageResponse.ret == -2`。
- `ret=-2` → 可重试；其它非零 ret → 普通错误（不无限重试）。
- 退避序列：起步 ~5s，×2，封顶 ~60s；绝不复用 ~10s 固定间隔（那是当前 bug 的元凶）。
- Rust 生产路径禁裸 `unwrap()`，错误传播用 `?` + `thiserror`/`anyhow`。
- commit 禁 `Co-authored-by`。
- 在 worktree 内开发，不直接动 main。

## 风险与回退

| 风险 | 影响 | 缓解 |
|---|---|---|
| partial 循环改造破坏现有时序 | 现有 e2e 全红 | 先用现有 `tests/e2e_wechat_simulation.rs` 做 baseline，改造后逐步对齐 |
| 退避计算散布到多处 | 行为不一致 | 抽出 `fn backoff_for(attempt: u32) -> Duration` 纯函数，单测钉死序列 |
| 永久限流导致任务挂死 | 任务协程泄漏 | M4 强制放弃上限 + 日志 |
| 缓冲 `pending` 在 task panic 时丢失 | 极端丢消息 | 退避窗口内 flush 失败时把 `pending` 保留在内存（不跨 panic 边界） |

## M5 实现说明（测试归属）

`tests/e2e_wechat_simulation.rs` 是 **Hub 侧** e2e：`MockUpstream` 模拟微信上游，
`bridge_send` 用裸 HTTP 模拟 bridge 转发，**并不运行 Bridge dispatcher 的重试循环**。
本 PR 的限流/退避/放弃逻辑全部位于 Bridge dispatcher 的 `run_partial_forward_loop` /
`send_final_with_retry`，二者及 `HubClient` 均为 `pub(super)`/私有，外部集成测试 crate
无法访问。因此按 plan 的 E2E-1「短路条件」与 M5「（或 bridge 单元测试）」约定，
"mock 上游先返回 N 次 `-2` 再成功" 的场景由 **in-crate dispatcher 单测**覆盖：

- `partial_three_throttles_then_success_delivers_latest_content` — partial 层 3×`-2`→成功，断言最新内容恰好送达一次、顺序保持。
- `final_reply_throttled_thrice_then_delivered` — 最终回复层 3×`-2`→成功，断言重试后送达。
- `final_reply_transport_error_propagates` — 非限流错误不无限重试，向上传播。
- `final_reply_persistent_throttle_gives_up_within_budget` / `partial_persistent_throttle_gives_up_then_serves_new_chunk` — M4 放弃上限。
- `final_reply_shutdown_during_backoff_returns_promptly` / `partial_shutdown_during_backoff_exits_cleanly` — cancel-safety。
- `backoff_sequence_matches_spec` / `partial_persistent_throttle_caps_retry_at_max_backoff` — 退避序列与墙钟收敛。

## 完成定义（Definition of Done）

- [x] 全部里程碑标记完成（M0–M4 实现 + M5 单测覆盖 + M6 质量闸）
- [x] E2E-0..E2E-5 验证项通过（M5 以 in-crate 单测形式覆盖，见上）
- [x] `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全绿（355 lib + 集成套件）
- [x] 完成标准复述（来自 prompt.md）逐条满足：`-2`→缓冲+退避重试、不丢消息、顺序保持、放弃上限+日志、退避起步 5s ×2 封顶 60s
- [ ] 在 worktree 内完成开发，待人工 review 后合并