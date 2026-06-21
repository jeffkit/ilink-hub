---
type: Reference
title: 环境变量配置
description: iLink Hub 所有环境变量配置项，包括数据库、监听地址、认证 Token 等。
resource: docs/reference/configuration.md
tags: [api, configuration, env, database]
timestamp: 2026-06-18T16:30:00+08:00
---

# 环境变量配置

iLink Hub 遵循 [12-Factor](https://12factor.net/config) 原则，所有配置通过环境变量注入。

## 核心变量

| 变量名 | 默认值 | 说明 |
|--------|--------|------|
| `DATABASE_URL` | `sqlite:~/.ilink-hub/ilink-hub.db` | 数据库连接字符串 |
| `WEIXIN_BASE_URL` | `http://0.0.0.0:8765` | Hub 监听/访问地址。**取代已废弃的 `ILINK_HUB_ADDR` / `ILINK_HUB_URL`** |
| `WEIXIN_TOKEN` | （未设置） | Bridge/SDK 连接 Hub 使用的虚拟 token（vtoken），由 `/hub/register` 或配对生成 |
| `ILINK_ADMIN_TOKEN` | （未设置） | 管理端点认证 Token，**生产环境必须设置** |
| `ILINK_HUB_MASTER_KEY` | （未设置） | **（必填）** 32 字节的 Base64 字符串或 Hex 字符串。用于 AES-256-GCM 静态加密 `bot_credentials.token`。缺失或格式错误进程将拒绝启动。 |
| `ILINK_TOKEN` | （未设置） | 跳过 QR 登录，直接使用已有的 iLink context_token（仅作引导种子，会写入 DB） |
| `ILINKHUB_RELAY_URL` | `https://ilinkhub.ai` | 公网配对中继地址 |
| `ILINKHUB_RELAY` | 启用 | 设为 `0` 禁用出站中继（仅本机调试用） |
| `HUB_PAIR_URL` | 自动 | 手动覆盖二维码公网前缀；设置后禁用自动中继（别名 `HUB_PUBLIC_URL`）|
| `HUB_CLIENT_URL` | `http://127.0.0.1:8765` | 配对成功后返回给 SDK 的 API 地址 |

> **已废弃变量**：`ILINK_HUB_ADDR`、`ILINK_HUB_URL` 仍被读取以兼容旧部署，但启动时会打印
> 废弃告警。请迁移到 `WEIXIN_BASE_URL`。

## 运维可选变量

| 变量名 | 默认值 | 说明 |
|--------|--------|------|
| `ILINK_QUEUE_BACKEND` | `memory` | 消息队列后端，目前仅支持 `memory`（`redis` 尚未实现，传入会启动失败）|
| `ILINK_MAX_QUEUE_SIZE` | `200` | 每个 vtoken 的内存队列上限，超出范围会被钳制并告警 |
| `ILINK_DISPATCH_CHANNEL_SIZE` | `1024` | 分发广播通道容量，过小会触发 Lagged 丢消息 |
| `ILINK_SHUTDOWN_DRAIN_SECS` | `30` | 优雅关闭时等待队列排空的最长秒数 |
| `ILINK_ADMIN_INSECURE_NO_AUTH` | 未设置 | 设为 `1` 关闭管理端点鉴权，**仅限本地调试，切勿用于生产** |

## 数据库连接格式

### SQLite（默认，无需额外编译 feature）

```bash
DATABASE_URL=sqlite:/path/to/ilink-hub.db
DATABASE_URL=sqlite:~/.ilink-hub/ilink-hub.db
```

### PostgreSQL（需编译时加 `--features postgres`）

```bash
DATABASE_URL=postgres://user:password@host:5432/database_name
```

> 仅支持 SQLite 与 PostgreSQL。MySQL 暂不支持（sqlx 的 `Any` 驱动不会重写 `$N`
> 占位符，而运行期 SQL 全部使用 `$N`，与 MySQL 的 `?` 协议不兼容）。
>
> 官方 Docker 镜像和预编译二进制已默认启用 SQLite 与 PostgreSQL 驱动。

## 相关文档

- [项目概览](/project/overview.md) — 技术栈整体介绍
- [常用命令速查](/dev-workflow/common-commands.md) — 带 feature flag 的编译命令
