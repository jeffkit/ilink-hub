# 配置 AI 客户端

注册客户端后，将 Hub 地址和虚拟 Token 填入你的 AI 客户端配置。客户端无需任何代码修改，仅需替换连接信息。

## Recursive

::: code-group

```toml [配置文件 (~/.recursive/config.toml)]
[weixin]
base_url = "http://your-hub.example.com:8765"
token = "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
```

```bash [环境变量]
export WEIXIN_BASE_URL=http://your-hub.example.com:8765
export WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
recursive weixin
```

:::

## OpenClaw

```json
// ~/.openclaw/openclaw.json
{
  "channels": {
    "weixin": {
      "base_url": "http://your-hub.example.com:8765",
      "token": "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    }
  }
}
```

## 任何基于 `wechatbot` Rust SDK 的客户端

```rust
let bot = WeChatBot::new(BotOptions {
    base_url: Some("http://your-hub.example.com:8765".to_string()),
    token: "vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".to_string(),
    ..Default::default()
});
```

## 通用方式（环境变量）

大多数兼容 iLink 协议的客户端都支持通过环境变量配置：

```bash
export WEIXIN_BASE_URL=http://your-hub.example.com:8765
export WEIXIN_TOKEN=vhub_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

## 验证连接是否正常

启动客户端后，在微信中发送任意消息，观察客户端是否能正常接收和回复。

也可以在微信中发送 `/list` 查看哪些客户端在线：

```
已注册客户端：
  ● mac-home（Mac 本机）—— 在线
  ○ server-prod（生产服务器）—— 离线
```

## 常见问题

### 客户端显示「连接失败」或「认证错误」

1. 确认 Hub 服务正在运行（访问 `http://your-hub:8765/health` 应返回 `{"status":"ok"}`）
2. 确认虚拟 Token 正确（注意不要多空格或少字符）
3. 如果设置了 `ILINK_ADMIN_TOKEN`，注册时需要携带认证头，见 [配置参考](/reference/configuration)

### 客户端能接收消息但发不出去

检查 Hub 日志，可能是 Token 映射问题。尝试重启 Hub 服务后再次测试。

### 多个客户端同时启动，消息只到一个

这是正常行为。iLink Hub 同一时间只将消息路由给**当前活跃客户端**。用微信命令 `/use <name>` 切换活跃后端，或使用 `/broadcast` 向所有在线后端广播。
