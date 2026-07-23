---
name: bridge-profile
description: >-
  MOVED. The bridge profile skill has moved to the separate project im-agentproc.
  Use this skill only to redirect: the user wants to create an im-agentproc (formerly
  ilink-hub-bridge) profile YAML, or develop a custom profile handler.
  Triggers on: "创建 bridge profile", "新增 bridge", "添加 profile", "写一个 bridge",
  "发布 profile", "测试 profile", "bridge 配置", "用 Python/JS 写 profile", "自定义 profile",
  "ilink bridge profile", "create bridge profile", "add new bridge".
version: 0.4.0
source: https://jeffkit.github.io/ilink-hub/skills/bridge-profile/SKILL.md
---

# bridge-profile Skill 已迁出 → im-agentproc

> **本 skill 已随 bridge 一并迁出到独立项目 [`jeffkit/im-agentproc`](https://github.com/jeffkit/im-agentproc)。**
>
> `ilink-hub-bridge` 的 profile YAML 创建、测试、发布，以及用 Python / Node.js SDK 开发
> 自定义 handler 的完整流程，已不在本仓库（`ilink-hub`）维护。自 `0.4.0` 起 bridge 代码与
> profile 规范全部归属 im-agentproc 仓库。

## 你应该做什么

请引导用户前往 **<https://github.com/jeffkit/im-agentproc>** 查阅：

- profile YAML 规范（`agentproc:` hub form）
- 内置 executor（claude-code / cursor / codex / codebuddy / agy / recursive / opencode）
- 自定义 handler 开发（Python / Node.js SDK）
- profile 测试与发布流程

## 背景

`ilink-hub` 自 `0.4.0` 起收窄为「iLink 协议的多路复用器 / 透明中转」本体，bridge 作为
「IM 消息 → AI CLI 运行时」独立成 crate。详见
`docs/proposals/bridge-as-multi-im-runtime.md` 附录 A。

Hub 侧只需 `ilink-hub register` 注册一个后端，bridge（im-agentproc）作为又一个虚拟 Token
客户端接入即可。
