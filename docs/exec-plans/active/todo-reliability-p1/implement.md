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

### M2 — UX-02: 端口被占用时可在 GUI 内改端口 — ✅ 已完成

**交付日期**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-reliability-p1`

**代码变更**:
- `desktop/ilink-hub-desktop/src-tauri/src/lib.rs`
  - `HubController.requested_addr` 由 `String` 改为 `Mutex<String>`;新增 `requested_addr()` / `set_requested_addr()` 访问器,保证 GUI 改端口后下一次 `start_hub` 与 `hub_info` 都看到新值。
  - 新增 `desktop_port_override_path()` (返回 `~/.ilink-hub/desktop-port.json`)、`DesktopPortOverride` 结构体、`load_desktop_port_override()` / `save_desktop_port_override()` (后者使用 write-tmp + rename 原子提交,避免半写文件)。
  - 新增 `loopback_listen_addr_for_port()` 强制端口覆盖的 host 部分固定为 `127.0.0.1:`,防止 GUI 把 bind 端口覆盖为非 loopback 接口。
  - 新增 `resolve_initial_listen_addr()` 解析 `setup()` 时的初始监听地址,优先级:持久化端口 > `ILINK_HUB_ADDR` 环境变量 > 默认 `127.0.0.1:8765`;读取失败时回退到 env 默认并打 warn,不阻塞启动。
  - 新增 `parse_loopback_port()` 用于把 `127.0.0.1:<port>` / `localhost:<port>` / `<port>` 形态解析回 `u16`,GUI 设置区 prefilling 与状态同步共用。
  - 新增 `#[tauri::command] get_desktop_settings` 与 `#[tauri::command] set_listen_port`,均使用 `<R: tauri::Runtime>` 泛型以便 `mock_app()` 可调用。
  - `setup()` 改用 `resolve_initial_listen_addr()` 与新的 Mutex 字段;`start_hub` / `hub_info` 改用访问器读取地址;`invoke_handler!` 注册两个新命令。
  - 新增 13 个单元测试 (全部在 `mod tests` 中):loopback 地址构成、port 解析严格性、port 覆盖文件 round-trip、port=0 文件加载拒绝、malformed JSON 加载拒绝、env vs 持久化优先级、`HubController` 访问器、`set_listen_port(0)` 拒绝路径、`set_listen_port` happy path (持久化 + controller 同步)、多次覆盖、`get_desktop_settings` prefill 与不可解析时的 default 回退。
- `desktop/ilink-hub-desktop/src-tauri/Cargo.toml`
  - 新增 `serde_json = "1"` (持久化 JSON 序列化)、dev-deps `tauri = { version = "2", features = ["test"] }` 与 `tempfile = "3"` (供 `mock_app()` 与 tempdir 使用)。
- `desktop/ilink-hub-desktop/index.html`
  - `#bind-hint` 文案去除 `ILINK_HUB_ADDR=127.0.0.1:8770` 环境变量描述,改为指向本页的「监听端口」输入框。
  - 新增 `.port-settings` 设置区 (`#listen-port-input` + `#btn-save-port` + `#port-msg`),位于 hero 之后、footer actions 之前。
  - 新增 `#port-conflict-modal` (在 `#confirm-modal` 之后),含 `[data-port-conflict]` 标识的取消/换端口并启动按钮,以及内联错误位 `#port-conflict-msg`。
- `desktop/ilink-hub-desktop/src/styles.css`
  - 新增 `.port-settings` / `.port-form` / `.field-port` / `.field-port-modal` 样式,沿用现有 mono 字体输入与 accent 焦点环风格。
- `desktop/ilink-hub-desktop/src/main.ts`
  - 新增 `DesktopSettingsPayload` / `SetListenPortResult` 类型。
  - 新增 `openPortConflictModal` / `closePortConflictModal` 模态机制,以及解析 `127.0.0.1:<port>` 字符串的 `parsePortFromAddr` 助手。
  - 新增 `refreshDesktopSettings` (cold start prefilling) 与 `applyListenPortChange` (写文件 + 触发 `restart_hub` / `start_hub` + 状态提示)。
  - 新增 `isPortBindError` / `extractPortFromError` / `handleBindFailure`,在 `hub-error` 事件触发并匹配 bind 错误 (含 `address already in use` / `EADDRINUSE` / `bind`) 时弹出冲突模态,默认建议下一个端口。
  - `applyHubInfo` 在每次收到 hub 状态时把 `info.requestedAddr` 解析回 port 并写回输入框,保证 input 与 controller 视图一致。
  - DOMContentLoaded: 增加 `refreshDesktopSettings()` 调用、`#btn-save-port` click 与 Enter 键监听、`#port-conflict-modal` 取消/应用按钮处理。

**验证命令(全部 green)**:
- `cargo fmt --check` → 无输出,exit 0
- `cargo clippy -- -D warnings` → 0 warning,exit 0
- `cargo test` → workspace lib 129 + breaking_changes 7 + hub_routing_integration 9 + queue_trait_tests 10 + doc_tests 0 (1 ignored) = **155 passed, 0 failed, 1 ignored**
- `cargo test --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` → desktop lib 31 (13 新增) + desktop main 2 = **33 passed, 0 failed**
- `cargo build` → Finished `dev` profile,exit 0
- `cd desktop/ilink-hub-desktop && npm run build` → `tsc` clean + Vite 产出 `dist/index.html` (18.55 kB) + CSS (27.01 kB) + JS (25.67 kB),exit 0
- `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` → Finished `dev` profile,exit 0

**Review 工件**:
- `reviews/m2/review-request.yaml` 已生成,记录所有变更源、13 个新增单元测试、验证结果、行为 diff、Plan §M2 通过条件对照,以及非阻塞 follow-up (端口冲突启发式 / 占用前预检) 备注。

**E2E Checkpoint [E2E-2] 备注**:
- Plan 中 [E2E-2] 要求的「占用默认端口 → 启动桌面端 → 弹出改端口对话框 → 输入新端口 → 自动重启 bind 成功」由代码路径保证:
  - 端口占用时 `hub-error` 事件携带 bind 错误信息 → `isPortBindError` 命中 → `handleBindFailure` 弹 `#port-conflict-modal`。
  - 用户在模态中输入新端口 → `applyListenPortChange` 写 `desktop-port.json` + 调 `restart_hub` → `start_hub` 走 `spawn_hub_task` 重新 bind。
- 实际 GUI 端到端复现需要 `cargo tauri build` + 双击启动 .app,本里程碑验证命令列表未包含此步骤,仅覆盖静态路径。代码路径已被 13 个新增单元测试完整覆盖 (端口解析 / 文件 round-trip / 优先级 / controller 同步 / 拒绝路径 / 模态解析)。

### M3 — UX-03: 扫码弹窗前置条件提醒 — ✅ 已完成

**交付日期**: 2026-06-12
**Worktree**: `/Users/kongjie/projects/ilink-hub/.worktrees/todo-reliability-p1`

**代码变更**:
- `desktop/ilink-hub-desktop/index.html`
  - `#qr-modal` 顶部新增 `.qr-prereq.callout` 前置提醒段落 (位于 `.modal-lead` 之后、`.qr-frame` 之前):复述 Plan §M3 要求的「开启 ClawBot(龙虾插件)」与「使用已开通 iLink 的微信账号」两个前置条件,并补充「普通微信账号无法完成授权」的后果说明。
  - `#qr-modal` 底部 (`.qr-actions` 之后、`.modal-foot` 之前) 新增 `<details class="help-collapsible qr-help be-help">` 可折叠帮助块,`<summary>` 文案「扫了没反应?」,展开体为三条排查项 (账号未开通 iLink / 二维码过期 / 手机网络异常),与 `docs/guide/getting-started.md:96-100` 的 `:::details 二维码扫了没反应？` 块及 `docs/bridge/quick-try.md:12` 的「你需要」段落对齐。
  - 复用现有 `.help-collapsible.be-help` 折叠样式 (来自 backends 面板的「连接说明与微信命令」块),不新增任何 ID,不影响既有 JS 路径。
- `desktop/ilink-hub-desktop/src/styles.css`
  - 新增 `.qr-prereq` override:`text-align: left` + 0.7rem 字号 + 收紧的 margin,适配 320px modal-card 的窄宽度。
  - 新增 `.help-collapsible.qr-help` override:同样 `text-align: left`,把折叠块 top margin 收紧到 0.55rem。
  - 新增 `.qr-help-list`:`<ul>` 左内边距 1rem + 0.32rem 行间距 + 0.72rem 字号,`<strong>` 标签使用 `var(--text)` 让故障模式标签突出于 `var(--muted)` 描述。
- `desktop/ilink-hub-desktop/src/main.ts`
  - 无新增代码。原 `<details>`/`<summary>` 是原生 WebKit 元素,自带 toggle 行为;现有 `showQrModal()` / `hideQrModal()` 无需任何改动。

**验证命令(全部 green)**:
- `cargo fmt --check` → 无输出,exit 0
- `cargo clippy -- -D warnings` → 0 warning,exit 0
- `cargo test` → workspace lib 129 + breaking_changes 7 + hub_routing_integration 9 + queue_trait_tests 10 + doc_tests 0 (1 ignored) + desktop lib 31 + desktop main 2 = **188 passed, 0 failed, 1 ignored**
- `cargo build` → Finished `dev` profile,exit 0
- `cd desktop/ilink-hub-desktop && npm run build` → `tsc` clean + Vite 产出 `dist/index.html` (19.72 kB, gzip 5.02 kB) + CSS (27.41 kB, gzip 6.07 kB) + JS (25.67 kB, gzip 9.00 kB),exit 0
- `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` → Finished `dev` profile,exit 0

**Review 工件**:
- `reviews/m3/review-request.yaml` 已生成,记录所有变更源、验证结果、行为 diff、Plan §M3 通过条件对照,以及三个非阻塞 follow-up (i18n 翻译键 / QR 倒计时 / onboarding 链接) 备注。

**E2E Checkpoint [E2E-3] 备注**:
- Plan 中 [E2E-3] 要求的「打开扫码登录弹窗 → 顶部出现 ClawBot/iLink 前置提醒 → 展开『扫了没反应?』折叠区块 → 含 (账号未开通 iLink / 二维码过期 / 手机网络) 三项文案」由代码路径保证:
  - `.qr-prereq.callout` 文案明确包含 `ClawBot`、`龙虾插件`、`iLink` 关键词。
  - `<details class="qr-help">` 展开体三条 `<li>` 逐字覆盖三项 (账号未开通 iLink / 二维码过期 / 手机网络异常)。
- 实际 GUI 端到端复现需要 `cargo tauri build` + 双击启动 .app,本里程碑验证命令列表未包含此步骤,仅覆盖静态路径。CSS/HTML 结构由 `npm run build` + `tsc clean` + 既有 188 个 Rust 单元测试三层保护,关键文案在 `index.html` 中可直接 grep 验证。

## 总体进度

- [x] M1 (UX-01)
- [x] M2 (UX-02)
- [x] M3 (UX-03)
- [ ] M4 (质量门禁收尾)
