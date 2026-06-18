---
type: Guide
title: 部署安全加固
description: 将 iLink Hub 安全地部署到生产环境的加固清单——鉴权、网络暴露、配对/中继安全、资源边界与上线前检查。
resource: docs/knowledge/ops/deployment-hardening.md
tags: [ops, security, deployment, hardening]
timestamp: 2026-06-18T16:30:00+08:00
---

# 部署安全加固

> 面向运维：把 iLink Hub 暴露到任何非 `127.0.0.1` 网络前，请逐项落实本清单。
> 配置项的完整说明见 [环境变量配置](../api/configuration.md)。

## 1. 信任边界

iLink Hub 有三条需要分别加固的暴露面：

| 暴露面 | 入口 | 主要风险 | 主要防护 |
|--------|------|---------|---------|
| 管理端点 | `/hub/*`（注册、客户端管理、二维码流） | 未授权管理操作、凭证泄露 | `ILINK_ADMIN_TOKEN` + 反向代理 TLS |
| 配对/中继 | `/pair/*`、出站到 `ILINKHUB_RELAY_URL` | 跨站配对劫持、中继伪造 | Origin/Referer 白名单、CSRF、Ed25519 中继密钥、限流 |
| Bridge 执行面 | 各 builtin CLI 子进程 | CLI 以 `--dangerously-skip-permissions` 运行，等同于在宿主机执行任意命令 | 进程/容器隔离、最小权限账户 |

## 2. 网络暴露与 TLS

- Hub 默认监听 `WEIXIN_BASE_URL`（`http://0.0.0.0:8765`），**明文 HTTP**。生产环境务必置于反向代理（Nginx / Caddy / 云 LB）之后，由代理终止 TLS。
- Hub 自身不做 TLS；不要把 `0.0.0.0:8765` 直接暴露到公网。
- 反向代理需正确透传 `Authorization` 头与 `Origin` / `Referer` 头（配对的跨站防护依赖它们）。
- 若仅本机调试，将监听地址收敛到 `127.0.0.1` 可直接消除大部分暴露面。

## 3. 管理端点鉴权（强制）

- **生产必须设置 `ILINK_ADMIN_TOKEN`**。设置后，所有管理端点要求 `Authorization: Bearer <token>`，且用**常量时间比较**（`subtle::ConstantTimeEq`）防时序侧信道。
- `ILINK_ADMIN_INSECURE_NO_AUTH=1` 会**完全关闭**管理鉴权，仅限本地调试；开启时启动日志会打印醒目告警。切勿在可被公网访问的实例上使用。
- 二维码登录流（SSE）走**一次性短时 ticket**：浏览器先用 admin token 调 `POST /hub/ilink/qr-stream-ticket` 换取 30 秒有效的一次性 ticket，再用 `?ticket=` 打开 `EventSource`。
  - 这样长效的 `ILINK_ADMIN_TOKEN` 不会出现在 `?token=` 明文 URL → 不落入代理访问日志、浏览器历史、`Referer`。
- Bridge / SDK 注册（`/hub/register`）同样需要与 Hub 一致的 `ILINK_ADMIN_TOKEN`；manager 模式下设置在 manager 进程环境即可，会自动透传给子 bridge。

> **凭证复用红线**：bridge 注册返回 401 时，**禁止**复用其它后端的 vtoken 绕过——共享 vtoken 会让多个 bridge 抢占同一消息队列（split-brain）。正确做法是补齐 `ILINK_ADMIN_TOKEN`。

## 4. 配对与中继安全

- **Origin/Referer 白名单**：配对确认类 POST 会校验 `Origin`（缺失时回退 `Referer`）必须匹配设备基址，拒绝跨站 drive-by 请求。代理改写 Host 时要保证该头一致。
- **CSRF**：配对会话绑定一次性 CSRF token，确认时用常量时间比较，用后即焚（防重放）。
- **中继密钥**：出站到 `ILINKHUB_RELAY_URL` 的请求用 Ed25519 / 共享密钥认证，校验同样是常量时间比较。
- **限流**：中继侧用固定窗口计数器（`relay::ratelimit`）对每个 key 限速，缓解暴力配对/枚举。
- 不需要公网配对时，设 `ILINKHUB_RELAY=0` 关闭出站中继，进一步缩小暴露面。

## 5. 凭证与日志

- 日志中的 vtoken / token 一律经 `redact_token` 脱敏（例如路由加载告警、`/session list` 回包的 backend 名回退）。新增日志涉及凭证时务必沿用脱敏。
- 不要把 token 放进 URL query；用 `Authorization` 头或一次性 ticket。
- `DATABASE_URL`、`ILINK_ADMIN_TOKEN` 等通过环境变量注入，不要写进镜像或提交到仓库。

## 6. 资源与拒绝服务边界

这些上限默认已生效，部署时确认未被调小到失效或调大到失去保护：

| 边界 | 默认 | 作用 |
|------|------|------|
| 每 vtoken 内存队列 | `ILINK_MAX_QUEUE_SIZE=200` | 防单租户消息堆积撑爆内存 |
| 分发广播通道 | `ILINK_DISPATCH_CHANNEL_SIZE=1024` | 过小会触发 Lagged 丢消息 |
| 每 vtoken 并发拉取 | 内置上限 | 防单租户耗尽拉取并发 |
| 中继 Hub 出站队列 | 256，满则返回 503 卸载 | 背压，防出站积压 OOM |
| 中继转发响应体 | 8 MiB 流式上限 | 防超大响应体 OOM |
| Bridge CLI 输出捕获 | 64 MiB（stdout/stderr，全路径含流式） | 防失控 CLI 无界增长内存 |
| 优雅关闭排空 | `ILINK_SHUTDOWN_DRAIN_SECS=30` | 关闭时等待队列排空上限 |

## 7. Bridge 执行面隔离

builtin handler 以 `--dangerously-skip-permissions` / `--yolo` 等参数运行底层 CLI（claude / codex / agent / agy），**等于授予在宿主机执行任意命令的能力**。因此：

- 用**专用低权限账户**或**独立容器**运行 bridge 进程，与 Hub、与宿主机其它服务隔离。
- 限制该账户的文件系统、网络与凭证可见范围（最小权限）。
- 不要把生产密钥/SSH key 暴露在 bridge 进程可读的环境里。
- 多后端场景下，每个 profile 独立注册、独立 vtoken、独立工作目录。

## 8. 数据库

- SQLite 文件（默认 `~/.ilink-hub/ilink-hub.db`）含会话与凭证，设置严格文件权限（如 `600`），不要置于多用户可读目录。
- 仅支持 SQLite 与 PostgreSQL；PostgreSQL 连接串中的密码同样只走环境变量。

## 9. 上线前检查清单

- [ ] 已设置强随机 `ILINK_ADMIN_TOKEN`，且未设置 `ILINK_ADMIN_INSECURE_NO_AUTH`
- [ ] Hub 置于反向代理之后，TLS 已终止，`Authorization` / `Origin` / `Referer` 正确透传
- [ ] 未将 `0.0.0.0:8765` 直接暴露公网
- [ ] 不需要公网配对时已 `ILINKHUB_RELAY=0`
- [ ] bridge 以独立低权限账户/容器运行，凭证范围最小化
- [ ] 数据库文件权限收紧，密钥仅经环境变量注入、未入仓库/镜像
- [ ] 资源边界（队列/通道/中继/CLI 捕获）保持默认或经评估的合理值
- [ ] 自定义日志均对 token/vtoken 脱敏

## 相关文档

- [环境变量配置](../api/configuration.md) — 所有配置项与默认值
- [项目概览](../project/overview.md) — 架构与组件
- [Bridge 概览](../bridges/overview.md) — Bridge / Profile / P0 协议
