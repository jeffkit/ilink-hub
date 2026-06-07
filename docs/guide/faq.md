# 常见问题 FAQ

## 安装与启动

### Q: 下载的二进制无法运行，提示「无法验证开发者」（macOS）

macOS Gatekeeper 会阻止未签名的第三方二进制。在终端运行：

```bash
xattr -rd com.apple.quarantine /usr/local/bin/ilink-hub
```

### Q: Linux 运行报错「glibc version not found」

当前预编译二进制要求 glibc 2.17+（CentOS 7+、Ubuntu 16.04+）。如果你的系统更老，建议：
- 使用 Docker 方式（不依赖宿主 glibc）
- 或从源码编译

### Q: 启动后无法访问 8765 端口

1. 检查防火墙/安全组是否开放 8765 端口
2. 确认监听地址是 `0.0.0.0:8765`（而不是 `127.0.0.1:8765`）
3. 验证 Hub 是否正常运行：`curl http://localhost:8765/health`

---

## 登录问题

### Q: QR 码扫码后提示「此二维码已过期」

iLink 登录二维码有效期较短（约 2 分钟）。重新启动 `ilink-hub serve`（会再次出码），或执行 `ilink-hub login` 仅更新凭证。

### Q: 登录成功但 Hub 启动后提示「upstream connection failed」

可能原因：
1. Token 已过期 → 再次运行 `ilink-hub serve` 完成扫码，或执行 `ilink-hub login`
2. 网络不通 → 确认服务器可以访问 `ilinkai.weixin.qq.com`
3. 数据库路径不一致 → 确认 `DATABASE_URL` 指向同一个数据库文件

---

## 客户端问题

### Q: 客户端显示在线但收不到消息

1. 在微信发送 `/list` 确认该客户端是否为当前活跃路由
2. 如果不是，发送 `/use <client-name>` 切换
3. 检查客户端日志是否有连接错误

### Q: 消息到了但回复失败（sendmessage 报错）

通常是 `context_token` 过期（微信 context_token 有时间限制）。这是正常现象，用户重新发消息后 Hub 会生成新的映射。

### Q: 多个客户端同时在线，消息为什么只发给一个？

这是设计行为。iLink Hub 同一时间只有一个「活跃客户端」接收消息，用微信命令 `/use <name>` 切换。如需同时发给所有客户端，用 `/broadcast <消息>`。

### Q: 注册时提示「name already exists」

客户端名称已存在。要么选一个不同的名称，要么先删除旧客户端（通过 Web UI 或 API）。

---

## 数据库

### Q: 数据库文件在哪里？

由 `DATABASE_URL` 决定。未设置时，Hub 默认在当前工作目录创建 **`./ilink-hub.db`**（SQLite）。Docker 部署示例中常为卷内的 **`/data/ilink-hub.db`**。

### Q: 可以迁移从 SQLite 到 PostgreSQL 吗？

目前需要重新登录和重新注册客户端（数据迁移工具尚未提供）。建议在一开始就选好数据库类型。

---

## 性能与稳定性

### Q: Hub 会因为消息队列满而崩溃吗？

不会。每个客户端的消息队列有上限（默认 200 条），超出时最旧的消息会被丢弃（head-drop 策略），并增加 `ilink_hub_messages_dropped_total` 计数器。服务不会崩溃。

### Q: Hub 崩溃后重启会丢失消息吗？

内存中的消息队列会丢失（尚未完成的、等待客户端取走的消息）。但 Token 映射、客户端注册等状态已持久化，重启后不需要重新配置。

### Q: 多个 Hub 实例可以同时运行吗？

目前不支持多实例（多个实例都会尝试独占真实的 iLink 连接）。单实例 Hub 已足够大多数用途。

---

## 其他

### Q: 支持群聊消息吗？

取决于微信 iLink API 本身的能力。iLink Hub 透明代理所有消息类型，只要原始 iLink API 支持的，Hub 都会转发。

### Q: 有 Windows 的 GUI 版本吗？

目前只有命令行工具。Web 管理面板（`/hub/ui`）可以在浏览器中操作，部分管理功能可以通过浏览器完成。

### Q: 开源协议是什么？可以商用吗？

MIT 协议，可以免费商用。详见 [LICENSE](https://github.com/jeffkit/ilink-hub/blob/main/LICENSE)。
