---
type: Reference
title: 微信命令参考
description: 在微信中控制 iLink Hub 路由的所有命令，包括 /list、/use、@name、/session、/broadcast 等。
resource: docs/reference/commands.md
tags: [api, commands, wechat, routing]
timestamp: 2026-06-18T10:00:00+08:00
---

# 微信命令参考

这些命令由 Hub 拦截处理，**不会**传递给 AI 后端。

## 命令速查

| 命令 | 功能 |
|------|------|
| `/list` | 查看所有已注册客户端及在线状态 |
| `/use <name>` | 切换活跃后端 |
| `@<name> <消息>` | 临时向某后端发消息（不改变当前路由） |
| `/session list` | 列出当前后端的所有 session |
| `/session new <名称>` | 新建 session |
| `/session use <名称>` | 切换活跃 session |
| `/session delete <名称>` | 删除 session |
| `/broadcast <消息>` | 向所有在线后端广播 |
| `/status` | 查看 Hub 运行状态 |

## 路由规则

- `/use <name>` 切换后，后续所有普通消息路由到该后端
- `@<name>` 是临时操作，**不改变** `/use` 状态，每次都新建会话
- **引用回复**某条机器人消息，会优先路由到发出该消息的后端及其 session

## 相关文档

- [环境变量配置](configuration.md) — Hub 服务器配置
- [Bridge 概览](/bridges/overview.md) — 理解"后端"概念
