# Feature: bridge-partial-throttle-retry

## 目标

修复一个**生产级丢消息 bug**：Bridge 把 AI 的流式分段回复（`ILINK_PARTIAL`）逐条发往 Hub，当微信上游触发限流、返回 iLink `ret=-2` 时，Bridge 现在只打一条 WARN 日志就**静默丢弃**该消息，且后续分段继续以高频盲发，导致：
1. 用户收不到限流之后的所有消息（一次长任务丢了 ~70 条）；
2. 高频盲发让微信的限流冷却期被无限延长。

本 feature 要把"遇 `ret=-2` 即丢弃"改成"**缓冲 + 合并 + 指数退避重试**"，保证消息**不丢**，并且重试节奏足够慢，不再加剧限流。

## 背景 / 关键约束（务必遵守）

- **架构事实**：Bridge 是 Hub 的一个 iLink 客户端，Bridge↔Hub 用的协议**与 Hub↔微信完全相同**（都是 iLink）。因此：
  - **绝对禁止引入任何新协议/新字段**。限流信号就复用现有的 `SendMessageResponse.ret == -2`（与微信限流时返回的完全一致）。
  - `ret=-2` 表示"被限流/暂时拒收"，是**可恢复**的；其它非零 ret 视为普通错误。
- 经实测，微信限流约为"发给同一用户 5 分钟 ~12 条"，且**被拒的 `-2` 请求本身仍消耗配额**；恢复需要约 5–7 分钟**降低发送频率**。因此重试退避必须**逐步拉长到 ~60s 量级**，绝不能像现状那样每 ~10s 猛重试（那正是把冷却期拖到 14 分钟的元凶）。

## 涉及代码（已定位）

- `src/bridge/dispatcher.rs`
  - `HubClient::sendmessage()`（约 90–117 行）：当前对任何 `ret != 0` 都 `anyhow::bail!`。需要让调用方能**区分** `ret == -2`（可重试限流）与其它错误。建议返回一个类型化结果（如内部 enum `SendOutcome { Sent, Throttled, Failed(anyhow::Error) }`，或自定义错误类型让调用方 `match`）。不要改 HTTP/JSON 结构。
  - partial 转发任务（约 530–548 行，读 `partial_rx` 的 `tokio::spawn`）：当前每收到一个 chunk 就发一次，失败 `warn` 后丢弃。改造成**有状态的缓冲循环**：
    - 维护本地 `pending: String`，把收到的 chunk **按到达顺序追加**进去；
    - 发送 `pending`（合并后的整段）；
    - 收到 `Throttled(-2)` → **不清空 `pending`**，按指数退避（起步 ~5s，每次 ×2，**上限 ~60s**）后**重试同一段**；退避期间继续从 channel 接收新 chunk 并追加进 `pending`（合并发送）；
    - 发送 `Sent` → 清空已发部分，退避计时器重置为起步值；
    - `Failed`（非 -2）→ 保持现有行为（记日志、跳过该段，不无限重试）。
  - 最终回复路径（约 565–591 行：`ILINK_PARTIAL`-only 导致 body 为空、转而持久化 `cli_session_id` 的分支，以及非空 body 的最终发送）：**同样**要在遇 `-2` 时缓冲重试，否则收尾消息/会话持久化会被吞掉（线上已发生）。

## 完成标准（可验证）

- [ ] 遇 `ret=-2` 时不再丢弃消息：被限流的分段会在退避后重试，**最终全部送达**（顺序保持）。
- [ ] 重试退避会逐步拉长到 ~60s 上限，不再固定高频重试。
- [ ] 退避/重试期间新到达的分段会合并进缓冲，一并发送，不丢。
- [ ] 限流恢复（mock 上游从返回 `-2` 切回成功）后，缓冲的积压内容被成功发出。
- [ ] 设置一个总的放弃上限（如与 profile 的 `timeout_secs` 关联，或一个合理的最大累计重试时长），永久限流时不会无限挂起，放弃时打清晰日志。
- [ ] 新增/扩展测试：在 `tests/e2e_wechat_simulation.rs`（已有可返回自定义响应的 mock `UpstreamSink`）或 bridge 单元测试中，构造"前 N 次 `sendmessage` 返回 `{"ret":-2}`、之后返回成功"的场景，断言被限流的内容**最终送达、且无丢失**。
- [ ] `cargo fmt --all -- --check` 通过。
- [ ] `cargo clippy --all-targets -- -D warnings` 零告警。
- [ ] `cargo test` 全绿。

## 非目标（本 PR 明确不做）

- **不做**分段聚合的"定时/按字数主动节流"和 **DeepSeek 小模型阶段汇总**（那是后续 feature ②，本 PR 只做"遇限流不丢、退避重试"的止血）。
- **不做** Hub 端的 per-peer 令牌桶 / `-2` 冷却短路（后续 feature ③）。
- **不改**版本号、**不做**任何部署。
- **不**引入新的 Bridge↔Hub 协议字段或新 HTTP 端点。
- **不**改动 `message_state` 语义（进展是否入历史等留到 ②）。

## 代码规范

- Rust 生产路径禁止裸 `unwrap()`，用 `?` + `thiserror`/`anyhow` 传播。
- commit 禁止添加 `Co-authored-by`。
- 在 force-dev 自动创建的 worktree 内开发，不在 main 直接提交。
