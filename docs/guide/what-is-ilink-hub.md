# 什么是 iLink Hub？

## 背景：微信 iLink API 的限制

微信 ClawBot 的 [iLink API](https://ilinkai.weixin.qq.com) 有一个排他锁机制：**同一时间只有一个进程可以轮询 `getupdates` 接口**。

这意味着如果你同时运行多个 AI 助手工具（比如 Mac 上的 Recursive、服务器上的 OpenClaw），它们会互相抢占同一条连接，最终只有一个能工作。

## 解决方案：透明代理

iLink Hub 是一个**透明的 iLink 协议代理**：

```
[微信用户]
    ↕ 真实 iLink 协议
[iLink Hub]  ← 唯一持有真实连接的进程
    ↕ 模拟 iLink API（相同 HTTP 端点、相同协议）
┌──────────────┐  ┌──────────────────┐  ┌───────────────┐
│Recursive(Mac)│  │Recursive(Server) │  │  OpenClaw 等  │
│base_url=hub  │  │base_url=hub      │  │  base_url=hub │
│token=vhub_aaa│  │token=vhub_bbb    │  │  token=vhub_ccc│
└──────────────┘  └──────────────────┘  └───────────────┘
```

Hub 作为唯一的真实连接持有者，向所有已注册的客户端**模拟 iLink API**（相同的 HTTP 端点、相同的请求/响应格式）。客户端完全感知不到代理的存在。

## 核心优势

| 特性 | 说明 |
|------|------|
| **零客户端改造** | 只需把 `WEIXIN_BASE_URL` 指向 Hub，换一个虚拟 Token，其他一行代码都不用改 |
| **多后端同时在线** | 所有注册的 AI 后端同时运行，通过微信命令或规则路由消息 |
| **Token 安全** | 真实 `context_token` 永不泄露，虚拟 Token 与真实 Token 的映射存储在数据库 |
| **持久化** | 客户端注册、路由状态、Token 映射在重启后保持不变 |

## 消息流转过程

1. 微信用户发送消息
2. Hub 轮询真实 iLink `getupdates` → 收到 `InboundMessage`
3. 路由器解析微信命令或决定目标客户端
4. 将真实 `context_token` → 虚拟 `context_token`（存入数据库）
5. 推送到目标客户端的队列（唤醒正在长轮询的 `getupdates` 请求）
6. 客户端的 `getupdates` 返回消息
7. 客户端处理后，用虚拟 `context_token` 调用 `sendmessage`
8. Hub 将虚拟 → 真实 `context_token`
9. Hub 转发 `sendmessage` 到真实 iLink
10. 微信用户收到回复 ✓

## 适合谁使用？

- 同时运行多个 AI 助手工具（Recursive、OpenClaw 等）的用户
- 需要在多台机器上共享同一个微信账号的开发者
- 希望给微信 AI 助手添加路由、管理功能的用户
