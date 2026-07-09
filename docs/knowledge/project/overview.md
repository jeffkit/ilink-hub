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
src/                Rust 核心服务
  server/           HTTP 路由与处理器（Axum）
  store/            数据库访问与迁移辅助
  hub/              Hub 状态、路由、队列、命令、配对
  bridge/           Bridge 实现（WebSocket 转发、内置 Bridge、dispatcher）
  ilink/            上游 iLink 协议客户端
  relay/            公网配对中继
  runtime/          进程启动与 serve 编排
  mcp/              MCP 相关适配
desktop/            Tauri 桌面应用（ilink-hub-desktop）
migrations/         SQLite/PostgreSQL 数据库迁移文件
tests/              集成测试
docs/               文档（本知识库也在这里）
docs/exec-plans/    四件套执行计划（active/进行中，completed/已归档）
journal/            每日工作日志
.flowx/             flowcast/force-dev 配置（质量门、agent chain）
```

## 核心概念关系

- **Hub** 管理上游 iLink 连接和下游客户端注册
- **Bridge** 是客户端的连接适配器，见 [Bridge 概览](/bridges/overview.md)
- **Profile** 是 Bridge 的运行时配置，见 [P0 协议与 Profile](/bridges/profile-protocol.md)
- **微信命令** 控制路由，见 [微信命令参考](/api/commands.md)
