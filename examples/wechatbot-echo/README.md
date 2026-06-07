# wechatbot Echo — iLink Hub 测试客户端

用 crates.io 上的 [`wechatbot`](https://crates.io/crates/wechatbot) SDK 连接本地 iLink Hub，验证「Hub ↔ AI 后端」整条链路。

## 架构

```
微信用户
   ↕ 真实 iLink（Hub 持有登录态）
iLink Hub（ilink-hub serve）
   ↕ /ilink/bot/* 兼容 API + vhub Token
本示例（终端二维码配对 → 长轮询回声）
```

## 零配置配对（推荐，OpenClaw 同款体验）

```bash
# 终端 1：Hub
ilink-hub serve

# 终端 2：Echo — 只需 Hub 地址
cd examples/wechatbot-echo
export WEIXIN_BASE_URL=http://127.0.0.1:8765
cargo run
```

终端会显示 **ASCII 二维码**（编码的是 `https://ilinkhub.ai/pair/...` 链接）。用手机扫描，在浏览器确认客户端名称，配对完成后自动开始长轮询。

无需 `WEIXIN_TOKEN`、`ilink-hub register` 或 Tunely。

### 重新配对

```bash
cargo run -- --force-pair
# 或删除凭证：rm .wechatbot-hub-credentials.json
```

## 手动 Token（可选）

```bash
ilink-hub register --name echo --label "echo test"
export WEIXIN_TOKEN=vhub_xxxxxxxx
cargo run
```

## 配置项

| 参数 / 环境变量 | 说明 | 默认值 |
|----------------|------|--------|
| `--hub-url` / `WEIXIN_BASE_URL` | Hub 地址 | `http://127.0.0.1:8765` |
| `--token` / `WEIXIN_TOKEN` | 跳过扫码，直接使用 vtoken | 无（走 QR 配对） |
| `--force-pair` | 强制重新扫码配对 | `false` |
| `--cred-path` | wechatbot 凭证缓存 | `.wechatbot-hub-credentials.json` |
| `--reply-prefix` | 自动回复前缀 | `Echo: ` |

## 微信测试

1. 给机器人发：`你好`
2. 应收到：`Echo: 你好`
3. 发 `/list`，Hub 应显示 echo 客户端在线

## 与 OpenClaw / Recursive 的关系

配对完成后，任何 iLink 客户端只需：

```bash
export WEIXIN_BASE_URL=http://127.0.0.1:8765
export WEIXIN_TOKEN=vhub_xxxxxxxx   # 扫码配对后自动获得
```

本示例的扫码逻辑在 `ilink_hub::client::pairing` 模块，其他 Rust 客户端可直接复用。

> **OpenClaw 插件**：消息 API 改 `base_url` 即可透明接入；首次 `channels login` 需确保插件对 `get_bot_qrcode` 使用配置的 Hub 地址（而非写死的 `ilinkai.weixin.qq.com`）。Hub 已兼容 POST 取码、`scaned` 状态与长轮询。
