# AGENTS.md — ilink-hub

ilink-hub 是一个 Rust 实现的 iLink 多端 Hub 服务，支持多 AI 客户端复用、桌面 Tauri 应用。
技术栈：Rust + SQLite/MySQL + Tokio + Tauri + TypeScript（桌面前端）。

## 知识库（OKF Bundle）

详细知识按主题结构化存放，**每次对话从这里导航，按需跳转**：

```
docs/knowledge/index.md    ← 入口，先读这里
```

| 主题 | 路径 | 包含内容 |
|------|------|---------|
| 项目概览 | `docs/knowledge/project/overview.md` | 仓库结构、技术栈 |
| 质量门 | `docs/knowledge/project/quality-gates.md` | CI 检查项与修复方式 |
| 代码规范 | `docs/knowledge/project/conventions.md` | Rust 规范、并发约定 |
| Bridge 概览 | `docs/knowledge/bridges/overview.md` | Bridge 架构与内置实现 |
| P0 协议 | `docs/knowledge/bridges/profile-protocol.md` | 环境变量契约、流式格式 |
| 微信命令 | `docs/knowledge/api/commands.md` | /list /use @name 等 |
| 环境变量 | `docs/knowledge/api/configuration.md` | DATABASE_URL 等配置 |
| force-dev | `docs/knowledge/dev-workflow/force-dev.md` | 启动/续跑 feature 分支 |
| 常用命令 | `docs/knowledge/dev-workflow/common-commands.md` | cargo 命令速查 |
| 发布部署 | `docs/knowledge/ops/release-and-deploy.md` | brew 发布三档路径、远程 Hub 部署 |
| 部署加固 | `docs/knowledge/ops/deployment-hardening.md` | 生产部署安全清单 |

## 活跃执行计划

见 `docs/exec-plans/active/` 目录。每次对话开始前读取对应 feature 的 `status.md` 恢复上下文。

## 必记规则（所有场景均适用）

- 代码变更前必须通过全部[质量门](docs/knowledge/project/quality-gates.md)
- 特性开发**禁止**在 main 分支直接提交，通过 force-dev worktree 隔离
- commit **禁止**添加 `Co-authored-by` 信息
- Rust 生产路径**禁止**裸 `unwrap()`，用 `thiserror` + `?` 传播
- 本地部署 hub/bridge **必须经 brew**（`/opt/homebrew/bin`）并**递增版本号**，**禁止** `deploy-local-mac.sh` 裸拷 `~/.local/bin` 覆盖。日常调试用 `scripts/deploy-local-brew.sh`（方案 2），patch 对外用 `v*-mac` tag（方案 1），minor/major 走完整 `release.yml`。详见[发布与部署规范](docs/knowledge/ops/release-and-deploy.md)

## 提交前检查清单

| 场景 | 必须执行的操作 |
|------|--------------|
| 修改任意 `.rs` 文件 | `cargo fmt --all -- --check`，不通过则先跑 `cargo fmt --all` |
| 修改任意 `.rs` 文件 | `cargo clippy -- -D warnings`，零 warning 才可提交 |
| 新增或升级 Rust 依赖 | `cargo update` 后将 `Cargo.lock` 一并提交（Docker 构建使用 `--locked`）|
| 修改 `sdk/node/src/` | **必须同步 bump `sdk/node/package.json` 的 `version` 字段**，否则 CI 会检测到版本已存在并跳过发布，新代码永远不会上线到 npm |
