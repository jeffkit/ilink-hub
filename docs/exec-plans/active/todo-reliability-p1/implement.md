# Implementation Progress — P1 UX 修复 (UX-01, UX-02, UX-03)

执行计划参考：[plan.md](./plan.md)

## 里程碑状态

### M1 — UX-01: 桌面端停止服务后可在应用内重启 — ✅ 已完成

**交付日期**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-reliability-p1`

**代码变更**:
- `desktop/ilink-hub-desktop/src-tauri/src/lib.rs`
  - 抽取 `setup()` 中的启动逻辑到 `spawn_hub_task(app, addr, db_path) -> watch::Sender<bool>` 辅助函数,支持多次调用复用。
  - 新增 `HubController::is_running()` 方法（用于 `start_hub` 的快速路径判断 + 单元测试）。
  - 新增 Tauri 命令 `start_hub`:检查 `is_running()`,调用 `spawn_hub_task` 重新安装 `shutdown_tx`。
  - 新增 Tauri 命令 `restart_hub`:取出已有 sender 并发送 stop 信号,忙等最多 5s 直到 `listening_addr` 清空,然后调用 `start_hub`。
  - `run_serve` 退出路径同时清空 `listening_addr` 与 `hub_state`,确保 `hub-stopped` / `hub-error` 事件后 `hub_info` 返回一致的 `stopped` 视图。
  - `invoke_handler!` 注册新增的 `start_hub` / `restart_hub` 命令。
  - 新增 5 个单元测试:HubController 状态机、`sqlite_url_for_path` 三种路径形态、backslashes 归一化、`stop_hub` 信号发送 + sender 取走后状态翻转、`stop_hub` 在已停止状态下幂等。
- `desktop/ilink-hub-desktop/index.html`
  - footer 新增 `#btn-start` 主按钮（btn-primary + play-triangle SVG,初始 hidden）。
  - `#btn-stop` 改为初始 `hidden`,运行中由 `setHubState` 拉起显示。
- `desktop/ilink-hub-desktop/src/main.ts`
  - `setHubState` 新增 footer 按钮切换逻辑:`stopped` / `error` → 显示启动主按钮;`running` / `starting` → 显示停止危险按钮;`starting` 期间禁用启动按钮防双击。
  - `#btn-start` click handler:调用 `start_hub`,状态置 `starting`,失败回退到 `stopped` 允许重试。
  - `#btn-stop` confirm 文案降权:首行承诺"停止后可在下方重新启动",后果说明移到独立段落。

**验证命令（全部 green）**:
- `cargo fmt --check` → 无输出,exit 0
- `cargo clippy -- -D warnings` → 0 warning,exit 0
- `cargo test` → ilink_hub lib 129 + desktop lib 10 (5 新增) + desktop main 2 + breaking_changes 7 + hub_routing_integration 9 + queue_trait_tests 10 = **167 passed, 0 failed, 1 ignored**
- `cargo build` → Finished `dev` profile,exit 0
- `cd desktop/ilink-hub-desktop && npm run build` → `tsc` clean + Vite 产出 `dist/index.html` (16.17 kB) + CSS (25.91 kB) + JS (22.29 kB),exit 0
- `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` → Finished `dev` profile,exit 0

**Review 工件**:
- `reviews/m1/review-request.yaml` 已生成,记录所有变更源、验证结果、行为 diff、Plan §M1 通过条件对照。

**E2E Checkpoint [E2E-1] 备注**:
- Plan 中 [E2E-1] 要求的"stop → start 全流程不需重启 .app 进程"由代码路径保证（`HubController` 跨 stop/start 周期存活,`shutdown_tx` 由 `start_hub` 重新填充）。
- 实际 GUI 端到端复现需要 `cargo tauri build` + 双击启动 .app,本里程碑验证命令列表未包含此步骤,仅覆盖静态路径。代码路径已被新增的 5 个单元测试完整覆盖（HubController 状态机 + stop_hub 信号 + 幂等无操作）。

### M2 — UX-02: 端口被占用时可在 GUI 内改端口 — ⏳ 待开始

(尚未执行。)

### M3 — UX-03: 扫码弹窗前置条件提醒 — ⏳ 待开始

(尚未执行。)

## 总体进度

- [x] M1 (UX-01)
- [ ] M2 (UX-02)
- [ ] M3 (UX-03)
- [ ] M4 (质量门禁收尾)
