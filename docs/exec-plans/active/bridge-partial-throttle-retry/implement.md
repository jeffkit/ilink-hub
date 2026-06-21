# Bridge Partial Throttle Retry Implement Log

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
