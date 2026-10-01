---
type: Project Overview
title: ilink-hub 项目概览
description: iLink 多端 Hub 服务，让一个微信账号同时接入多个 AI 客户端。
tags: [project, architecture, rust]
timestamp: 2026-07-09T16:00:00+08:00
---

# ilink-hub 项目概览

ilink-hub 是一个 Rust 实现的反向代理 Hub，让单个微信账号同时被多个 AI 客户端（Claude Code、Cursor、OpenClaw 等）复用。

## 技术栈

| 层 | 技术 |
|----|------|
| 核心服务 | Rust + Tokio（异步） |
| HTTP/WS | Axum |
| 数据库 | SQLite（默认）/ PostgreSQL，通过 sqlx |
| 桌面应用 | Tauri + Vite + TypeScript |
| 错误处理 | thiserror |
| 锁 / 并发 | `tokio::sync`、`std::sync`、`DashMap`、`arc_swap`（`parking_lot` 仅作传递依赖，非主路径） |

## 仓库结构

```
src/                Rust 核心服务（lib + bins）
  server/           HTTP 路由与处理器（Axum）
  store/            数据库访问与迁移辅助（SQLite / PostgreSQL）
  hub/              Hub 状态、路由、队列、命令、配对
  ilink/            上游 iLink 协议客户端
  relay/            公网配对中继
  runtime/          进程启动与 serve 编排、静态加密（AES-256-GCM）
  mcp/              MCP 相关适配
  client/           AI 后端接入 Hub 的配对客户端
  bin/              独立二进制（ilink-relay）
desktop/            Tauri 桌面应用（ilink-hub-desktop）
deploy/             部署配置样例（Docker Compose、relay systemd/nginx）
migrations/         SQLite/PostgreSQL 数据库迁移文件
tests/              集成测试
docs/               文档（本知识库也在这里）
docs/exec-plans/    四件套执行计划（active/进行中，completed/已归档）
examples/           客户端接入示例
sdk/                SDK（Node 等）
scripts/            部署与检查脚本
demo/               演示录像（gif/tape）
journal/            每日工作日志
.flowx/             flowcast/force-dev 配置（质量门、agent chain）
```

> 注：原 `src/bridge/` 已于 2026-07-20 物理拆分到独立仓库
> [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)（crate `im-agentproc`），
> 本仓不再包含 bridge 代码；desktop 经 crates.io 引用 `im-agentproc` 获取 bridge。
> Bridge 相关文档保留为概念参考（见 [Bridge 概览](/bridges/overview.md)）。

## 核心概念关系

- **Hub** 管理上游 iLink 连接和下游客户端注册
- **Bridge** 是客户端的连接适配器，见 [Bridge 概览](/bridges/overview.md)
- **Profile** 是 Bridge 的运行时配置，见 [P0 协议与 Profile](/bridges/profile-protocol.md)
- **微信命令** 控制路由，见 [微信命令参考](/api/commands.md)
