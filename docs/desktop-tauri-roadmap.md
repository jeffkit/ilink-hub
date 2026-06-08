# iLink Hub 桌面版（Tauri）路线图

**目标**：在保留现有命令行与库行为的前提下，增加可选的桌面壳（托盘、配置、日志、二维码展示）。

**约束**：

- 根目录包 `ilink-hub` 的默认构建、发布、CI **不得**因桌面子工程而失败或变慢。
- 桌面子工程 **独立目录**、**独立构建**；未加入根 `[workspace]` 前，在仓库根执行 `cargo build` / `cargo test` 与现在完全一致。
- CLI 子命令（`serve` / `login` / `register` / `clients`）逻辑以 **单一事实来源** 为准：优先抽到 `ilink_hub` 库或共享模块，桌面与 CLI 共用，避免双份实现。

**最后更新**：2026-06-08

---

## 阶段 0：仓库布局与 CI 约定（无行为变更）

| 任务 | 说明 | 状态 |
|------|------|------|
| 0.1 | 使用 `desktop/` 存放 Tauri 应用（建议最终结构见下文） | 已完成 |
| 0.2 | **不要**在阶段 0～1 把 `desktop/` 加入根 `Cargo.toml` 的 `[workspace]` | 已完成 |
| 0.3 | CI（`.github/workflows`）默认 **不** 构建 `desktop/`，或单独 job、可 `paths-filter` 触发 | 无需变更（`desktop/` 无独立 crate） |

---

## 阶段 1：从 `main.rs` 抽出「启动服务」API（行为对齐 CLI）— **已完成**

Hub 启动逻辑已迁入 `ilink_hub::runtime::serve`，由 CLI 与未来的 Tauri 共用。

| 任务 | 说明 | 状态 |
|------|------|------|
| 1.1 | 新增 `src/runtime/mod.rs`、`src/runtime/serve.rs`，迁入 `run_serve`、`resolve_token`、`load_clients_from_db`、`build_queue_backend` | 已完成 |
| 1.2 | `main.rs` 的 `Serve` 分支仅组装 `ServeOptions` 并调用 `ilink_hub::run_serve`（在 `lib.rs` 中 re-export） | 已完成 |
| 1.3 | `run_serve(opts, shutdown_rx: watch::Receiver<bool>)`：由调用方持有 `watch::Sender`，在 Ctrl+C（CLI）或应用退出（桌面）时 `send(true)`；上游轮询与 Axum graceful shutdown 共用同一 `Receiver` | 已完成 |
| 1.4 | `tracing_subscriber` 仍在 `main` 初始化；`run_serve` 文档注明调用方负责 logging | 已完成 |

**注意**：`resolve_token` 内仍为终端 `println!` 与 QR 终端输出；阶段 4 再抽象「登录 UI 回调」。

---

## 阶段 2：初始化 Tauri 应用 — **已完成（MVP）**

工程路径：`desktop/ilink-hub-desktop/`（Vite + TypeScript 前端，`src-tauri` 依赖仓库根 `ilink-hub`）。

| 任务 | 说明 | 状态 |
|------|------|------|
| 2.1 | 脚手架 + `npm run tauri dev` / `tauri build` 可运行 | 已完成 |
| 2.2 | `src-tauri` 中 `ilink-hub = { path = "../../../" }`，`setup` 内 `tauri::async_runtime::spawn(run_serve)` | 已完成 |
| 2.3 | 简单窗口：展示监听地址、DB 路径、打开 `/hub/ui`、停止 Hub | 已完成 |

后续增强（托盘、内嵌二维码等）见阶段 3～4。

---

## 阶段 3：集成 Hub 生命周期与托盘

| 任务 | 说明 | 验收 |
|------|------|------|
| 3.1 | 已在 MVP 中通过 `watch` + 关窗 / `RunEvent::Exit` 触发 `run_serve` 优雅退出；可再收紧「等待 Axum 完全退出」 | 部分完成 |
| 3.2 | 默认 `127.0.0.1:8765`（`ILINK_HUB_ADDR` 可覆盖） | 已完成 |
| 3.3 | 系统托盘 | 待做 |
| 3.4 | SQLite 使用 `dirs::data_local_dir()/ilink-hub-desktop/ilink-hub.db` | 已完成 |

---

## 阶段 4：前端与登录体验

| 任务 | 说明 | 验收 |
|------|------|------|
| 4.1 | 简单页：服务状态、监听地址、`DATABASE_URL` 只读或高级设置 | 可本地打开 |
| 4.2 | 日志：`tracing` 订阅层把事件推到 Tauri event / 前端只读区域，或轮询小日志 API（若后续加管理接口） | 可观察启动错误 |
| 4.3 | QR 登录：扩展 `LoginClient` 或 `resolve_token` 支持「生成 PNG / data URL」回调，在 WebView 展示；或桌面内嵌 `ilogin` 子命令输出到临时 HTML | 无需依赖终端扫码即可完成首次绑定 |
| 4.4 | 与 `/hub/ui` 关系：桌面主窗口为**紧凑原生 UI**（状态 + 客户端列表 + 插件占位）；完整注册 / 复制 Token 等仍在浏览器 `/hub/ui`。列表通过本机 HTTP `GET /hub/clients` 拉取（未设置 `ILINK_ADMIN_TOKEN` 时免鉴权；已设置时需为桌面进程配置相同环境变量） | 已写入本文档 |

---

## 阶段 5：发布与文档

| 任务 | 说明 | 验收 |
|------|------|------|
| 5.1 | 在 **推送 `v*` tag** 的 `release.yml` 中增加 `build-desktop` job：构建 `ilink-hub-desktop-{macos-aarch64,macos-x86_64,windows-x86_64,linux-x86_64}.{dmg,msi,deb}` 并随 Release 上传 | Release Assets 含 `ilink-hub-desktop-*` |
| 5.2 | 文档站 [安装](/guide/installation#desktop) + 首页入口；根 `README` 指向 Releases 桌面包 | 公众可从文档/GitHub 获取安装包 |

---

## 建议目录结构（完成后）

```text
ilink-hub/
  src/
  desktop/
    README.md                 # 入口说明
    ilink-hub-desktop/        # Tauri 2 工程（npm + src-tauri）
      README.md
      package.json
      src-tauri/
  docs/
    desktop-tauri-roadmap.md
```

若日后希望「一条命令构建全部」，再引入根级 `[workspace]` members，并明确默认 `cargo build -p ilink-hub` 用于核心发布。

---

## 风险与依赖

- **Tokio**：Tauri 侧需确认 Runtime 策略（单全局 runtime vs 在 `std::thread` 中 `block_on`），避免与 Axum 阻塞模型冲突；建议查阅 Tauri 2 + tokio 官方示例。
- **二维码**：终端 `qrcode` 与 GUI 展示分离，避免在库内强绑定 `stdout`。
- **安全**：桌面版内置管理 UI 时，勿弱化 `ILINK_ADMIN_TOKEN` 等现有安全模型。

---

## 与当前代码的映射

| 位置 | 说明 |
|------|------|
| `src/runtime/serve.rs` | `ServeOptions`、`run_serve`；内部含 `resolve_token`、`load_clients_from_db`、`build_queue_backend` |
| `src/main.rs` | `Cli` / `Commands`；`Serve` 分支创建 `watch::channel`、注册 Ctrl+C、`run_serve(...).await` |
| `src/lib.rs` | `pub mod runtime`；`pub use runtime::serve::{run_serve, ServeOptions}` |
| `src/bridge/mod.rs` | `ilink-hub-bridge`：以虚拟 token 连接 Hub，对每条文本消息执行配置的本地 CLI（见 `docs/bridge/README.md`） |

桌面/Tauri：`ilink_hub = { path = ".." }`，在 `setup` 中 `tokio::spawn` 调用 `run_serve`，并保留 `shutdown_tx` 供菜单「退出」触发停机。

此文档随阶段推进更新；后续阶段以 PR 为准。
