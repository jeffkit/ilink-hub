---
type: Concept
title: Bridge 概览
description: Bridge 是 ilink-hub 的客户端适配层，每种 AI 工具对应一种 Bridge 实现（AgentProc 0.4 NDJSON 协议）。
tags: [bridge, architecture, agentproc]
timestamp: 2026-07-13T20:30:00+08:00
---

# Bridge 概览

Bridge 是 ilink-hub 的**客户端适配层**：Hub 通过 Bridge 与各种 AI 工具通信，将微信消息转发给对应的 AI 后端并把回复传回用户。Bridge 与 profile 进程之间采用 **AgentProc 0.4** NDJSON 协议（见 [P0 协议与 Profile](profile-protocol.md)）。

## 内置 Bridge

| Bridge | 文件 | 适配目标 |
|--------|------|---------|
| `cursor` | `src/bridge/builtin/cursor.rs` | [Cursor Bridge](cursor.md) |
| `claude-code` | `src/bridge/builtin/claude_code.rs` | [Claude Code Bridge](claude-code.md) |
| `codebuddy-code` | `src/bridge/builtin/codebuddy_code.rs` | CodeBuddy Code CLI |
| `codex` | `src/bridge/builtin/codex.rs` | OpenAI Codex CLI |
| `agy` | `src/bridge/builtin/agy.rs` | Google Antigravity CLI |
| `recursive` | `src/bridge/builtin/recursive.rs` | [Recursive Bridge](recursive.md) |

## Bridge 的工作方式

1. AI 工具向 Hub 注册（WebSocket 连接 + 认证）
2. 用户在微信发消息 → Hub 路由到当前活跃 Bridge
3. Bridge 将消息转换为目标工具的格式并发送
4. Bridge 读取工具响应，实时流式转发回微信

## Profile 机制

每个 Bridge 的运行参数由 **Profile** 配置。Profile 定义了要启动的进程、工作目录、环境变量等。详见 [P0 协议与 Profile](profile-protocol.md)。

## Bridge 模块结构

```
src/bridge/
├── mod.rs              # Bridge trait 定义与注册
├── protocol.rs         # AgentProc 0.4 wire 协议：TurnObject / AgentEvent / PermissionResponse
├── executor.rs         # 进程编排：写 NDJSON turn 到 stdin、解析 NDJSON 事件、permission 通道
├── config.rs           # BridgeProfile / BridgeConfig schema（env_allowlist / permission / kill_grace_secs 等）
├── probe.rs            # profile 健康探针（0.3 NDJSON turn）
├── dispatcher/         # 消息分发与回复装配
└── builtin/
    ├── agy.rs          # Google Antigravity CLI Bridge
    ├── claude_code.rs  # Claude Code Bridge 实现
    ├── codebuddy_code.rs # CodeBuddy Code CLI Bridge
    ├── codex.rs        # OpenAI Codex CLI Bridge
    ├── common.rs       # 共享工具函数（读 stdin turn、emit NDJSON 事件）
    ├── cursor.rs       # Cursor Bridge 实现
    └── recursive.rs    # Recursive Agent CLI Bridge
```

## 相关文档

- [AgentProc 0.4 协议与 Profile](profile-protocol.md) — NDJSON turn/事件契约
- [微信命令](/api/commands.md) — `/use <name>` 切换 Bridge
