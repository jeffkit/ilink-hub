# SDK 兼容性与推进动态

> 最后更新：2026-06-07

iLink Hub 的多客户端复用依赖 AI 客户端能把**所有** iLink 请求（包括二维码登录）指向 Hub 地址（如 `http://127.0.0.1:8765`）。是否「零改造」取决于该 SDK 是否允许配置 `base_url` / `WEIXIN_BASE_URL`。

本页汇总各 SDK 的兼容情况，以及我们为打通它们向上游提交的改动进度。

## 兼容性总览

| SDK / 客户端 | 类型 | 消息 API 可配 `base_url` | 二维码登录可配 `base_url` | 接 Hub 现状 |
|---|---|---|---|---|
| `ilink_hub::client::pairing`（本项目） | Rust SDK | ✅ | ✅ | ✅ 开箱即用 |
| [epiral/weixin-bot](https://github.com/epiral/weixin-bot) | Python / Node | ✅ | ✅ | ✅ 配置 `base_url` 即可 |
| [photon-hq/wechat-ilink-client](https://github.com/photon-hq/wechat-ilink-client) | TypeScript | ✅ | ✅ | ✅ 配置 `base_url` 即可 |
| [zongrongjin/weixin-ilink](https://github.com/zongrongjin/weixin-ilink) | Python | ✅ | ✅ | ✅ 配置 `base_url` 即可 |
| Recursive | 闭源 CLI | ✅ | ✅ | ✅ 支持 `WEIXIN_BASE_URL` |
| [corespeed-io/wechatbot](https://github.com/corespeed-io/wechatbot) | Rust/Go/Node/Python | ✅ | ⏳ 待合并 | ⏳ 需上游 PR + 发版 |
| [Tencent/openclaw-weixin](https://github.com/Tencent/openclaw-weixin) | TypeScript SDK | ✅ | ⏳ 待合并 | ⏳ 需上游 PR + 发版 |
| CodeBuddy / WorkBuddy | 腾讯闭源桌面应用 | — | ❌ 无公开配置 | ❌ 暂不支持，详见下文 |

::: tip 「✅ 配置 `base_url` 即可」是什么意思？
这些 SDK 并不都内置 `WEIXIN_BASE_URL` 环境变量，但都允许在初始化时传入 `base_url`（或等价参数）。只需把它指向 Hub 地址即可，无需改 SDK 源码。
:::

## 推进动态（上游 PR）

部分 SDK 的**消息 API** 已尊重 `base_url`，但**二维码登录**（`get_bot_qrcode` / `get_qrcode_status`）写死了腾讯域名（`ilinkai.weixin.qq.com`），导致无法通过本地 Hub 完成零配置扫码配对。我们为此提交了上游修复：

| PR | 仓库 | 状态 | 内容 |
|---|---|---|---|
| [#83](https://github.com/corespeed-io/wechatbot/pull/83) | corespeed-io/wechatbot | ⏳ 待 review/合并 | Rust / Go / Node / Python 四处 QR 登录改用配置的 `baseUrl`，移除写死的 `FIXED_QR_BASE_URL` |
| [#190](https://github.com/Tencent/openclaw-weixin/pull/190) | Tencent/openclaw-weixin | ⏳ 待 review/合并 | `login-qr.ts` 的 `fetchQRCode` / 轮询 / 刷新改用 `opts.apiBaseUrl` |

合并后还需各仓库**发版**，用户升级到新版本即可对这些 SDK 实现完整零配置接入。

## CodeBuddy / WorkBuddy 说明

[CodeBuddy](https://www.codebuddy.cn/) IDE 与 WorkBuddy 是腾讯的**闭源桌面应用**，通过 Claw 设置内的「微信助理集成 / 微信客服号集成」做 UI 内扫码绑定。

它们与上面的开源 SDK 不同：

- **没有公开配置项**：官方文档只描述应用内扫码，未提供 `WEIXIN_BASE_URL` / 自定义 iLink 地址 / Hub 地址等设置。
- **闭源**：无法作为 SDK 引用，也无法像 `wechatbot-echo` 那样改几行代码指向 `http://127.0.0.1:8765`。
- **底层推测**：很可能内嵌 `@tencent-weixin/openclaw-weixin` 或同类实现；即便 [PR #190](https://github.com/Tencent/openclaw-weixin/pull/190) 合并，仍需产品侧发版并在设置中**开放 endpoint 配置**，用户才能指向 Hub。
- **`~/.codebuddy/models.json`** 只配置大模型 API，与微信 iLink 通道无关。
- **企微 AI Bot**（`CODEBUDDY_WECOM_*`）是企微开放平台机器人，**不是**个人微信 iLink，与 iLink Hub 无关。

**结论**：CodeBuddy / WorkBuddy 今天**不能**纳入 iLink Hub 的零配置多客户端方案。要接入需要腾讯产品侧支持（在 Claw/助理设置中增加「自定义 iLink 服务地址」或识别 `WEIXIN_BASE_URL`），不是单靠 Hub 或上游 SDK PR 就能解决。

## 我该选哪个？

- **想立刻接 Hub**：用 `ilink_hub::client::pairing`、Recursive、OpenClaw，或任意可配 `base_url` 的开源 SDK（epiral、photon-hq、zongrongjin 等）。
- **在用 wechatbot / openclaw-weixin 做二维码登录**：等上述 PR 合并 + 发版，或暂时用我们的 fork。
- **在用 CodeBuddy / WorkBuddy**：暂走腾讯官方直连，关注产品侧是否开放自定义服务地址。
