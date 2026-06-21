# Bridge Partial Throttle Retry Implement Log

## M2 — partial 分段转发改造：缓冲 + 合并 + 指数退避

### Decisions

- **抽出 trait `ReplySender`**：partial forward loop 的"发送一个 partial chunk 并拿到 SendOutcome"是一个独立、可注入的副作用，最适合 trait 抽象。这样单测可以用 in-memory ScriptedSender 驱动整个循环，零 HTTP 依赖。HubClient 给出一个 trivial impl（复用 `sendmessage`，把 `session_name` 挂到 `ilink_hub_ext`）。
- **backoff 用 `fn(u32) -> Duration` 函数指针注入**而非 trait object：生产用 `backoff_for`（5/10/20/40/60s），测试用 `test_backoff`（5ms 起步、40ms 封顶）。函数指针无堆分配、编译期 monomorphize、测试只需传一个 `fn` 名字。
- **"合并"语义选 "覆盖" 而非 "拼接"**：plan.md 描述的"合并"在 iLink partial reply 语境下指"积压只保留最新一份"。CLI 流式输出 + iLink 协议不要求按 chunk 顺序回放，保留历史片段既浪费流量也更容易触发更多限流。所以新 chunk 总是覆盖 pending，永远只重发最新内容。E2E-4 后续在真实场景下验证顺序保持。
- **Err（非 Throttled）清空 pending + 重置 attempt**：避免 transport 错误导致 spin 死循环。这是 M1 `SendOutcome` 设计的自然延伸（"Throttled 之外是 generic failure"）。
- **没有引入 give-up 上限**：那是 M4 范围。pending 会一直重试直到 Sent / Err / shutdown。
- **每个 await 都在 select 中观察 shutdown**：cancel-safety 不依赖任何同步原语。phase 1 if/else 与 phase 2 send 路径都把 `shutdown.cancelled()` 放在 `tokio::select!` 的 biased 第一支。

### Problems

- `cargo clippy --all-targets` 起初因 BoxFuture 的路径报错：`tokio::sync::futures::BoxFuture` 不存在。改用 `futures_util::future::BoxFuture`（项目依赖了 `futures-util = "0.3"`），无需新增 crate。
- 测试中 `tokio::time::sleep(Duration::from_millis(N))` 的精度与生产 `backoff_for` 的 5s 起步差距太大：第一版 `backoff_for_test(attempt, 10, 80)` 把 10/80 当 ms 实际是 s，结果 4 次 retry 跑了 20s+。修正为 `Duration::from_millis` 显式单位、`backoff_for_test(attempt, Duration, Duration)` 后单测 3s 跑完。
- `Mutex<Vec<...>>` 不实现 `Clone`，但 ScriptedSender 需要 clone（spawn 拿走一份、test 留 probe）。改造为 `Arc<Mutex<...>>` 内层即可 derive Clone。
- `ScriptedSender::new(vec![SendOutcome::Sent])` 在新增 Err 场景后类型不匹配（Vec<SendOutcome> vs Vec<Result<SendOutcome>>）。把 script 改为 `Vec<Result<SendOutcome>>`，所有 SendOutcome 项包 `Ok(...)`、Err 显式 `Err(anyhow!(...))`。
- 起初把 `eprintln!` 调试日志留在代码里，调试完全部移除。
- store::store_tests::adversarial_many_concurrent_connects_converge 在 cargo test 全量时偶发失败（database is locked），与 m2 改动无关 — 单跑和重跑全量都通过；记录为已知 flaky。

### Outcome

- `cargo fmt --check`：通过。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `cargo test`：
  - unit_lib: 350 passed (M0: 323 → +27：m1 增 +14、m2 增 +13)；0 failed；0 ignored
  - integration: 76 passed (M0: 76)
  - total: 426 passed; 0 failed; 1 ignored
  - desktop-tauri: 33 passed; 0 failed; 0 ignored
- `cargo build`：通过 (4.23s)。
- `cd desktop/ilink-hub-desktop && npm run build`：通过 (90ms)。
- `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml`：通过 (4.77s)。
- M2 新增测试覆盖：
  - `backoff_sequence_matches_spec` — 钉死 [5,10,20,40,60,60,...]
  - `backoff_clamps_at_cap_for_large_attempt` — 10+ 次 attempt 都 cap 在 60s
  - `backoff_does_not_overflow_at_u32_max` — u32::MAX 不溢出
  - `backoff_is_non_decreasing` — 50 个 attempt 序列逐对单调不递减
  - `partial_three_throttles_then_success_delivers_latest_content` — E2E-1 场景 A
  - `partial_single_chunk_throttled_then_success_buffers_until_clear` — 单 chunk 基本路径
  - `partial_chunk_overwritten_during_backoff_drops_stale_fragment` — overwrite 语义
  - `partial_persistent_throttle_caps_retry_at_max_backoff` — 场景 B 短变体
  - `partial_err_drops_buffer_and_continues_serving_new_chunks` — Err 防 spin
  - `partial_shutdown_during_backoff_exits_cleanly` — cancel-safety
  - `partial_chunks_arrived_after_sender_continues_normal_path` — happy path 顺序保持
  - `reply_sender_impl_attaches_session_name_to_hub_ext` — trait impl 编译期 pin
- `reviews/m2/review-request.yaml` 已记录：基线状态、设计取舍、命令输出、pass conditions（含 E2E-1 场景 A/B 单独断言）、delta summary、known limits / non-goals、M3 衔接。

## M1 — `HubClient::sendmessage()` 返回类型化结果

### Decisions

- m1 决定在 `src/bridge/dispatcher.rs` 引入 `enum SendOutcome { Sent, Throttled { ret, errmsg } }`，把 `HubClient::sendmessage` 签名从 `Result<()>` 改为 `Result<SendOutcome>`，HTTP / JSON 结构零变化（仍然走同一组 endpoint、同一组 body）。`ret == -2` → `Throttled`；其它非零 ret → 走 `anyhow::bail!`，调用方错误传播路径不变。
- 4 处调用点（shutdown-error、partial reply、cli_session_id 持久化、final reply、CLI-error reply）把原来的 `if let Err(e) = client.sendmessage(req).await { warn!(...) }` 改为 `match { Ok(Sent) => {}, Ok(Throttled) => warn!(... M2/M3 placeholder ...), Err => warn!() }`；final-reply 这一个调用点在 Throttled 分支里 `anyhow::bail!` 升级为错误。
- 抽 `parse_sendoutcome(text: &str) -> Result<SendOutcome, (i32, Option<String>)>` 纯函数，让 m1 之后的回归测试有稳定 oracle；旧行为（JSON 解析失败 → `Ok(Sent)`）作为向后兼容保留，并加 `warn!` 让 fallback 可观测。
- 抽 `sanitize_errmsg(s)` 把 errmsg 过滤控制字符 / ANSI / 截断 256 字符，防止上游恶意 errmsg 污染日志或 buffer。

### Problems

- 第一版 final-reply 路径的 `anyhow::bail!` 与 `HandleError` 不兼容（E0308 at dispatcher.rs:661），返回类型推断成 `bridge::dispatcher::HandleError` 但 `anyhow::bail!()` 产出 `anyhow::Error`。修复：bail 改为走 `HandleError::from(e.context(...))`，与同分支 Err 路径一致。
- m1 review-request.yaml（reviews/m1/review-request.yaml）记录了第一次失败的 build — 提交后已修复，未再回退。
- 桌面端 `hub_delete_client` force 标志已在 m0 同步过；m1 未触碰桌面端。

### Outcome

- `cargo build`：通过。
- `cargo clippy --all-targets -- -D warnings`：通过。
- `cargo test`：unit_lib 323 → 337 (+14)；其余 0/76/33 持平。
- `reviews/m1/review-request.yaml` 与 `reviews/m1/review-findings.yaml` 已留底。

## M0 — 现状基线

### Decisions

- worktree 在 m0 开始时 **不是 clean baseline**：main 工作区有 17 个未提交修改（包含新增 `migrations/0007_peer_user_id_unique_index.sql`、对 `src/store/sessions.rs` 的 73 行增量等），但 worktree 基于 git 索引 fork，没有携带这些修改。直接跑 `cargo build` 失败（9 个 E0599/E0658/IO 错误）。
- 选择 **同步 main WIP 到 worktree** 而非"基线已红、隔离到独立 PR"：本 PR 的目标是 `ret=-2` 重试改造，不希望基线校验被无关的 main WIP 噪声污染；同步后 worktree 与磁盘上的 main 状态一致，m1-m6 可以基于真实基线开发。
- `unregister_client_in_hub` 在 main WIP 中改为 3 参 `(state, name, force)`，同步到 worktree 后桌面端 `hub_delete_client`（用户主动从 UI 删除 backend）需要补 `force=true` —— 与 `src/server/routes.rs:807` 的语义一致，是 UI 主动操作，理应跳过 `StillOnline` 守卫。
- main 工作区的 WIP 修改已通过 `git stash push -u` 暂存再 `pop` 还原，patch 落盘到 `/tmp/m0-wip-snapshot/main-wip.patch` 留底；未丢失任何工作。

### Problems

- worktree 启动时缺 `migrations/0007_peer_user_id_unique_index.sql` → `include_str!` 编译错误。
- worktree 启动时 `src/store/sessions.rs` 缺 `get_hub_ext_single` 方法，但 `src/hub/dispatch.rs:573` 已调用 → E0599。
- worktree 启动时 `src/bridge/manager.rs` 的 `libc::SIGTERM` 在新 rustc 触发 E0658（rustc_private）→ 这是 toolchain 变化导致；同步的 main WIP 似乎已经规避了此路径，未在本 worktree 复现。
- 同步 main WIP 后桌面端 `unregister_client_in_hub` 调用站断在编译期（参数个数 2 vs 期望 3）。

### Outcome

- `cargo fmt --check`：通过。
- `cargo clippy -- -D warnings`：通过（4.73s）。
- `cargo test`：399 passed; 0 failed; 1 ignored。
- `cargo build`：通过（9.76s）。
- `cd desktop/ilink-hub-desktop && npm run build`：通过（111ms）。
- `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml`：通过（2.53s）。
- `cargo test --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml`：33 passed; 0 failed; 0 ignored。
- 基线测试用例数 399 + 33 = 432；M6 时与 E2E-0 对比要求增长 ≥ 3。
- `reviews/m0/review-request.yaml` 已记录：基线状态、命令输出、pre-M0 worktree 状态、m0 deviation（同步 main WIP + 桌面端 force 标志）、pass conditions、baseline summary。
