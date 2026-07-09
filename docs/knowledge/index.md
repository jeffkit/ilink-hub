# ilink-hub Knowledge Bundle

> OKF v0.1 bundle — 供 AI agent 和人类开发者导航使用。
> 每次对话从这里出发，按需跳转到具体概念，无需加载全量文档。

## 项目

* [项目概览](project/overview.md) — 仓库结构、技术栈、目标定位
* [质量门](project/quality-gates.md) — 每次代码变更必须通过的 CI 检查
* [代码规范与约定](project/conventions.md) — Rust 规范、并发开发约定

## Bridge

* [Bridge 概览](bridges/overview.md) — Bridge 是什么、有哪些内置实现
* [P0 协议与 Profile](bridges/profile-protocol.md) — 进程契约：环境变量输入、stdout 输出、流式格式
* [Cursor Bridge](bridges/cursor.md) — Cursor IDE bridge 实现细节
* [Claude Code Bridge](bridges/claude-code.md) — Claude Code CLI bridge 实现细节

## API & 命令

* [微信命令](api/commands.md) — /list、/use、@name、/session、/broadcast 等
* [环境变量配置](api/configuration.md) — DATABASE_URL、ILINK_HUB_ADDR 等所有配置项

## 开发工作流

* [force-dev 工作流](dev-workflow/force-dev.md) — 用 force-dev 启动/续跑 feature 分支
* [常用命令速查](dev-workflow/common-commands.md) — cargo test/clippy/fmt/build 等
* [变异测试](../mutation-testing/README.md) — cargo-mutants 基建、基准与持续推进节奏

## 运维与安全

* [发布与部署规范](ops/release-and-deploy.md) — 三档发布路径（本机 brew / mac-fast / 完整 CI）与远程 Hub 部署
* [部署安全加固](ops/deployment-hardening.md) — 鉴权、网络暴露、配对/中继安全、资源边界与上线前检查清单

## 执行计划

见 [`docs/exec-plans/active/`](../exec-plans/active/) 目录，每个 feature 有独立的 `status.md`。
