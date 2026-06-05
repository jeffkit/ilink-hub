# 与同类项目对比

## 功能对比

| 项目 | 客户端协议 | 多机器支持 | 零改造 | 独立部署 |
|------|-----------|-----------|--------|---------|
| **iLink Hub**（本项目） | ✅ iLink 兼容 | ✅ 是 | ✅ 是 | ✅ 是 |
| OpeniLink Hub | ❌ 自定义 WebSocket/SDK | ✅ 是 | ❌ 需改代码 | ✅ 是 |
| HermesClaw | ❌ 仅本地代理 | ❌ 否 | ❌ | ✅ 是 |
| wechat-clawbot | HTTP webhook | ✅ 是 | ❌ 需改代码 | ✅ 是 |
| OpenClaw bindings | ❌ OpenClaw 特定 | ❌ 同一台机器 | ❌ | ✅ 是 |

## 核心差异

**iLink Hub 的独特优势**在于完全兼容 iLink 协议：

- **零客户端改造**：只需修改 `BASE_URL` 和 `TOKEN` 两个环境变量，无需修改任何代码
- **协议透明**：客户端不知道自己在和代理通信，就像直连真实 iLink API
- **完整协议支持**：`getupdates`、`sendmessage`、`sendtyping`、`getconfig`、`getuploadurl` 全部支持

## 选择建议

- 如果你已经在用 Recursive 或 OpenClaw，想让它们同时跑在多台机器上 → **用 iLink Hub**
- 如果你在开发自己的 iLink 应用，需要一个测试多实例的环境 → **用 iLink Hub**
- 如果你需要完全自定义的路由逻辑，不介意改客户端代码 → 考虑 OpeniLink Hub
