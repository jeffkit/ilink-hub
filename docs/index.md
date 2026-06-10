---
layout: home

hero:
  name: iLink Hub
  text: 一个微信账号，连接多个 AI 助手
  tagline: 让 Claude Code、Recursive、OpenClaw 等多个 AI 工具同时接入同一个微信，随时切换，互不干扰。
  image:
    src: /logo.png
    alt: iLink Hub
  actions:
    - theme: brand
      text: 让 AI 帮我安装 ✨
      link: /guide/ai-install
    - theme: alt
      text: 下载桌面版
      link: /guide/installation#desktop
    - theme: alt
      text: 命令行快速开始
      link: /guide/getting-started

features:
  - icon: 🔀
    title: 多个 AI 同时在线
    details: 家里的 Claude、公司的 Cursor、服务器上的 OpenClaw，全部同时接入同一个微信，在微信里发 /use 切换，一秒到位。
  - icon: 📱
    title: 零改造接入
    details: 只需把 AI 客户端的服务器地址换成 Hub 地址，其他什么都不用改。Recursive、OpenClaw 等工具开箱即用。
  - icon: 🔒
    title: 凭证安全
    details: 你的真实微信 Token 只留在 Hub 里，各 AI 工具拿到的是 Hub 颁发的虚拟凭证，互相隔离。
  - icon: 🖥️
    title: 桌面应用
    details: 提供 macOS / Windows / Linux 桌面版，双击安装，扫码登录，无需终端。也提供命令行版本供服务器部署。
  - icon: 💬
    title: 微信命令控制
    details: 在微信中发送 /list 查看所有 AI、/use <名称> 切换活跃 AI、/broadcast 向全部 AI 广播消息。
  - icon: 🗄️
    title: 数据持久化
    details: 切换记录、会话上下文全部存入数据库，重启 Hub 后无需重新配置。默认 SQLite，也支持 PostgreSQL。
---

<div style="max-width: 760px; margin: 64px auto 0; padding: 0 24px;">

## 最快的方式：让 AI 帮你装

AI 时代不用自己逐条看文档。把下面这句话**直接发给你的 AI 助手**（Claude Code、Cursor 等），它会读取 iLink Hub 的安装 Skill，自主完成全部配置：

<div style="background: var(--vp-c-bg-soft); border: 1px solid var(--vp-c-border); border-radius: 12px; padding: 20px 24px; margin: 16px 0;">

**发给 Claude Code / Cursor：**

```
请读取 https://jeffkit.github.io/ilink-hub/skills/ilink-hub-setup/SKILL.md，
然后帮我完成 ilink-hub 的安装与配置。
```

</div>

AI 会引导你确认操作系统、芯片型号、微信是否已开启 ClawBot，然后自动完成安装、扫码绑定、启动 bridge 的全部步骤——无需你查任何文档。

→ [查看更多 AI 安装方式（Cursor / 本地安装 / 典型对话示例）](/guide/ai-install)

---

## 不用 AI？选择适合你的方式

使用前需在微信中开启 **ClawBot（龙虾插件）**：微信 → 「我」→「设置」→「插件」，找到 ClawBot 开启即可，无需申请。

| 我的情况 | 推荐路径 |
|----------|---------|
| 不想用终端，想要图形界面 | [下载桌面版](/guide/installation#desktop) |
| 熟悉终端，想快速部署 | [快速开始](/guide/getting-started) |
| 想先试试效果，再决定怎么部署 | [5 分钟上手（echo）](/bridge/quick-try) |
| 想把微信接到 Claude Code | [接入 Claude Code](/guide/claude-code) |

</div>
