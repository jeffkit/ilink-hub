---
type: Concept
title: Bridge 概览
description: Bridge 是 ilink-hub 的客户端适配层，每种 AI 工具对应一种 Bridge 实现（AgentProc 0.4 NDJSON 协议）。
tags: [bridge, architecture, agentproc]
timestamp: 2026-07-16T14:30:00+08:00
---

# Bridge 概览

Bridge 是 ilink-hub 的**客户端适配层**：Hub 通过 Bridge 与各种 AI 工具通信，将微信消息转发给对应的 AI 后端并把回复传回用户。Bridge 与 profile 进程之间采用 **AgentProc 0.4** NDJSON 协议（见 [P0 协议与 Profile](profile-protocol.md)）。

## 内置 Executor

| Executor | 适配目标 |
|----------|---------|
| `claude-code` | [Claude Code Bridge](claude-code.md) |
| `cursor` | [Cursor Bridge](cursor.md) |
| `codebuddy` | CodeBuddy Code CLI |
| `codex` | OpenAI Codex CLI |
| `agy` | Google Antigravity CLI |
| `recursive` | [Recursive Bridge](recursive.md) |
| `opencode` | OpenCode CLI |

实现位于 `src/bridge/builtin/`；运行时优先经 `agentproc::run` 驱动（见 `dispatcher/agentproc_runner.rs`）。

## Bridge 的工作方式

1. Bridge 子进程向 Hub 注册（虚拟 token）
2. 用户在微信发消息 → Hub 路由到当前活跃客户端
3. Bridge 将消息写成 AgentProc turn，经 executor / spawn 交给 CLI
4. Bridge 读取 NDJSON 事件，按 `streaming` hint 转发回微信

## Profile 机制

**一个 YAML 文件 = 一个 Hub 客户端 = 一个 agentproc profile。**  
执行配置嵌在 `agentproc:` 下；详见 [P0 协议与 Profile](profile-protocol.md)。多后端请用 manager 多文件 + Hub `/use`，不再在单文件内做 prefix 路由。

## Bridge 模块结构

```
src/bridge/
├── mod.rs                 # 模块入口
├── protocol.rs            # AgentProc 0.4 wire：TurnObject / AgentEvent / PermissionResponse
├── executor.rs            # 进程编排（legacy spawn 路径）
├── config.rs              # BridgeProfileFile / AgentprocBlock / BridgeApp（单 profile）
├── probe.rs               # profile 健康探针
├── manager.rs             # profiles 目录 → 多子进程
├── dispatcher/
│   ├── agentproc_runner.rs  # BridgeProfile → agentproc::run
│   ├── handle.rs            # 入站过滤 + 错误回执
│   └── session.rs           # 按 session 串行派发
└── builtin/               # 各 CLI 的 in-process / spawn 适配
```

## 相关文档

- [AgentProc 0.4 协议与 Profile](profile-protocol.md) — NDJSON turn/事件契约与 YAML hub form
- [微信命令](/api/commands.md) — `/use <name>` 切换 Bridge
