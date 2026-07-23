---
title: Bridge 已迁出
description: ilink-hub-bridge 已拆分到独立项目 im-agentproc
---

# Bridge 已独立为 im-agentproc

「微信消息 → 本机 CLI（Claude Code / Cursor / Codex 等）」的 bridge 能力已从
`ilink-hub` 仓库物理拆分到独立项目 **[`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)**。

| 项 | 值 |
|----|----|
| 仓库 | <https://github.com/jeffkit/im-agentproc> |
| crate | `im-agentproc`（crates.io） |
| 二进制 | `im-agentproc`（原 `ilink-hub-bridge`） |
| 独立 Homebrew formula | 由 im-agentproc 仓库自行发布 |

## 为什么拆

`ilink-hub` 收窄为「iLink 协议的多路复用器 / 透明中转」本体；bridge 作为「IM 消息 →
AI CLI 运行时」正交演进，独立成 crate 后可同时服务多种 IM、多种 agentproc profile，
不再与 Hub 服务耦合。详见
[`docs/proposals/bridge-as-multi-im-runtime.md`](https://github.com/jeffkit/ilink-hub/blob/main/docs/proposals/bridge-as-multi-im-runtime.md)
附录 A。

## 现在怎么用 bridge

1. 在 Hub 侧正常 `ilink-hub register --name <name>` 注册一个后端，拿到 `vhub_…`。
2. 前往 [im-agentproc](https://github.com/jeffkit/im-agentproc) 安装 `im-agentproc`，
   配置 profile YAML，`WEIXIN_BASE_URL` 指向 Hub、`WEIXIN_TOKEN` 用上一步的 vtoken。
3. 微信里 `/use <name>` 切到该后端即可。

> 本页仅作跳转。原 `docs/bridge/` 下的使用指引、profile 规范、SDK 开发教程、示例 YAML
> 已随 bridge 一并迁入 im-agentproc 仓库，请到那边查阅最新版本。
