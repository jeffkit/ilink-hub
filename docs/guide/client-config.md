# 配置 AI 客户端

## 首次接入：终端二维码配对（推荐）

只需配置 Hub 地址，客户端首次启动会显示终端二维码，手机扫码确认即可获得 `vhub_` Token：

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
# 不设 WEIXIN_TOKEN — 走扫码配对
```

Rust 可复用 `ilink_hub::client::HubPairingClient`；或直接运行 `examples/wechatbot-echo` 体验完整流程。详见 [手机扫码配对](./pairing-tunnel.md)。

## 已有 Token 时

注册或配对完成后，将 Hub 地址和虚拟 Token 填入 AI 客户端。消息收发无需改代码，仅需替换连接信息。

::: tip 不确定你的 SDK 是否支持？
各 SDK 对 Hub 的兼容情况、以及我们为打通它们提交的上游 PR 进度，见 [SDK 兼容性与推进动态](./sdk-compatibility.md)。
:::

## 本地 CLI bridge（已独立为 im-agentproc）

原官方 `ilink-hub-bridge` 与上面客户端**协议相同**：虚拟 Token + `getupdates` / `sendmessage`，
但每收到一条**用户文本**就在本机 `spawn` YAML 里配置的命令，把 **stdout** 发回微信。

适合：把 **Claude Code、Codex、自写脚本** 接到微信，又不需要改 Hub 代码。

> **已拆分**：自 `0.4.0` 起，`ilink-hub-bridge` 拆到独立项目
> [jeffkit/im-agentproc](https://github.com/jeffkit/im-agentproc)（crate `im-agentproc`，
> bin `im-agentproc`）。安装、profile 配置、示例 YAML 请到该仓库查阅。

与 Recursive / OpenClaw **可同时注册**：多占一个 `--name`，用微信 `/use` 在「大模型客户端」和「CLI bridge」之间切换。

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

## 用 wechatbot 快速验证（Echo 示例）

仓库自带一个最小测试客户端，使用 crates.io 上的 [`wechatbot`](https://crates.io/crates/wechatbot) SDK：

```bash
# 1. 注册客户端
ilink-hub register --name echo --label "echo test"

# 2. 运行示例（把 token 换成上一步输出）
cd examples/wechatbot-echo
export WEIXIN_BASE_URL=http://localhost:8765
export WEIXIN_TOKEN=vhub_xxxxxxxx
cargo run
```

微信发 `你好`，应收到 `Echo: 你好`。详见 [`examples/wechatbot-echo/README.md`](https://github.com/jeffkit/ilink-hub/tree/main/examples/wechatbot-echo)。

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
