# iLink Hub（桌面版）

基于 **Tauri 2** 的桌面壳：在应用内启动与 CLI `ilink-hub serve` 相同的 [`ilink_hub::run_serve`](../../src/runtime/serve.rs) 运行时。

## 面向使用者的安装包

正式发布时，维护者在仓库根推送 **`v*`** 版本 tag 后，GitHub Actions 会在 [Releases](https://github.com/jeffkit/ilink-hub/releases) 中上传 `ilink-hub-desktop-macos-aarch64.dmg`、`ilink-hub-desktop-macos-x86_64.dmg`、`ilink-hub-desktop-windows-x86_64.msi`、`ilink-hub-desktop-linux-x86_64.deb` 等 Assets（与 `ilink-hub` CLI 二进制同一流水线）。文档站说明见 [安装 — 桌面应用](https://jeffkit.github.io/ilink-hub/guide/installation.html#desktop)。

## 环境

- [Rust stable](https://rustup.rs/)
- Node 20+（用于 Vite / `npm run tauri`）

## 常用命令

```bash
cd desktop/ilink-hub-desktop
npm install

# 开发（热更新前端 + 本机 Hub）
npm run tauri dev

# 发布构建（产物在 src-tauri/target/release/bundle/）
npm run tauri build
```

## 行为说明

- **界面**：主窗口分 **「首页」** 与 **「后端」** 两个 Tab。首页展示运行状态、Hub 地址（`WEIXIN_BASE_URL`），以及从 `/metrics` 拉取的简要统计（已注册后端数、在线比例、转发消息数、微信侧进入 Hub 的消息数）。**「后端」** Tab 将「注册」与「已接入列表」合并在同一区块，底部用 **「? 如何连接与微信命令」** 可折叠说明。底栏 **停止服务** 为危险操作样式，点击后会二次确认再执行停机。
- **监听地址**：默认 `127.0.0.1:8765`，可通过环境变量 `ILINK_HUB_ADDR` 覆盖（与 CLI 一致）。只有真正 `bind` 成功后首页才显示 Hub 地址并允许注册/拉取后端；若 CLI 已占用同端口，绑定会失败。若申请绑定地址与实际监听不一致（例如 `0.0.0.0`），首页会简短标注内核监听信息。
- **完整管理页**：与 CLI 相同，仍可通过浏览器访问本机 Hub 的 `/hub/ui`（桌面端不再提供入口链接）。
- **数据库**：SQLite 文件位于各平台用户本地数据目录下的 `ilink-hub-desktop/ilink-hub.db`（与 CLI 默认的当前目录 `./ilink-hub.db` 不同，避免「从哪启动」导致库文件漂移）。
- **微信扫码**：桌面版会在窗口内弹出二维码（无需看终端）；仍可用「复制备用链接」在微信里打开。
- **退出**：关闭窗口或应用退出时会向 Hub 发送优雅停机信号（`watch::Sender`）。

## 与仓库根包的关系

本目录 **未** 加入仓库根 `Cargo.toml` 的 `[workspace]`；在仓库根执行 `cargo build` / `cargo test` 仍只构建 CLI 与库，不受影响。
