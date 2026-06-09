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
      text: 我不懂代码，下载桌面版
      link: /guide/installation#desktop
    - theme: alt
      text: 我会用终端，快速开始
      link: /guide/getting-started
    - theme: alt
      text: 在 GitHub 查看
      link: https://github.com/jeffkit/ilink-hub

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

<div style="max-width: 720px; margin: 48px auto; padding: 0 24px;">

## 使用前，你需要准备一件事

iLink Hub 依赖微信官方的 **iLink（ClawBot）接口**——这是微信开放给 AI 工具的专属通道，需要单独申请开通。

> **还没有 iLink 账号？** 前往 [ilinkai.weixin.qq.com](https://ilinkai.weixin.qq.com) 按照官方指引申请，通常需要 1-3 个工作日审核。

已经有 iLink 账号了？选择下面适合你的方式开始：

| 我的情况 | 推荐路径 |
|----------|---------|
| 不想用终端，想要图形界面 | [下载桌面版](/guide/installation#desktop) |
| 熟悉终端，想快速部署 | [快速开始](/guide/getting-started) |
| 想先试试效果，再决定怎么部署 | [5 分钟上手（echo）](/bridge/quick-try) |
| 想把微信接到 Claude Code | [接入 Claude Code](/guide/claude-code) |

</div>
