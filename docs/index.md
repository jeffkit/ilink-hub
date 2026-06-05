---
layout: home

hero:
  name: iLink Hub
  text: 一个微信账号，连接多个 AI 后端
  tagline: 透明代理，零客户端改造。让 Recursive、OpenClaw 等多个 AI 工具同时接入同一个微信账号。
  image:
    src: /logo.svg
    alt: iLink Hub
  actions:
    - theme: brand
      text: 快速开始
      link: /guide/getting-started
    - theme: alt
      text: 在 GitHub 查看
      link: https://github.com/jeffkit/ilink-hub

features:
  - icon: 🔀
    title: 透明代理
    details: 完整实现 iLink API 协议，任何已有的 iLink 客户端（Recursive、OpenClaw 等）无需修改代码，只需更换 BASE_URL 和 TOKEN 即可接入。
  - icon: 🔒
    title: 安全隔离
    details: 真实的 context_token 永不暴露给客户端，虚拟 Token 翻译在 Hub 内部完成并持久化到数据库，重启不丢失会话状态。
  - icon: 📊
    title: 可观测
    details: 内置 Prometheus 指标端点 `/metrics`，健康检查 `/health`，以及 Web 管理面板 `/hub/ui`，让服务运行状态一目了然。
  - icon: 🐳
    title: 一键部署
    details: 提供预编译二进制（Linux/macOS/Windows）和 Docker 镜像，无需安装 Rust 环境，一行命令即可启动。
  - icon: 💬
    title: 微信命令控制
    details: 在微信中发送 /list、/use、/broadcast 等命令，随时切换活跃后端，无需操作服务器。
  - icon: 🗄️
    title: 多数据库支持
    details: 默认使用 SQLite，也支持 PostgreSQL 和 MySQL，通过 DATABASE_URL 一个环境变量切换，无需改代码。
---
