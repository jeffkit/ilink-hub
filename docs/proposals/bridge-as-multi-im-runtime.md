# 提案：将 Bridge 升级为多 IM 协议运行时

| 项 | 值 |
|---|---|
| 状态 | **Draft — 关键决策已定，待实施评审** |
| 作者 | jeffkit |
| 创建 | 2026-07-16 |
| 更新 | 2026-07-16 |
| 关联 | `docs/proposals/project-aware-backends.md`（Backend/Project 分层）、`agentproc` crate（AI CLI 适配层） |

---

## 1. 目标与非目标

### 目标

- 让 `src/bridge/` 成为**与 IM 协议无关的通用「IM 消息 → AI CLI」运行时**。
- **iLink（微信 clawbot）降级为其中一种协议适配**，与飞书 / 钉钉 / Telegram / Slack 等并列。
- 第一阶段**不物理拆仓库**：在 `ilink-hub` crate 内完成接口抽象与解耦，验证可行后再考虑抽成独立 crate（对齐 `agentproc` 的演进路径）。

### 非目标（本提案不解决）

- 不在本提案新增任何具体非微信 IM 适配器（飞书/Telegram 等仅作为扩展点预留）。
- 不改变 Hub 的多端复用 / 路由 / session 持久化职责。
- 不在本提案做物理仓库拆分（阶段 0–2 全部在 crate 内）。

## 2. 现状（已纠正）：bridge 已经在直接说 iLink 协议，Hub 是透明中转

经核实，Hub 对 bridge 暴露的下游端点复用的就是 iLink 协议本身：

```src/server/routes/bot.rs
getupdates  →  Json<GetUpdatesRequest>  →  Json<GetUpdatesResponse>   # 均为 crate::ilink::types::* 
sendmessage →  Json<SendMessageRequest> →  Json<SendMessageResponse>
```

`src/ilink/types.rs` 对 `ilink_hub_ext` 的注释明确：「ilink-hub 扩展元数据（Hub 与已注册后端之间专用，**不会透传给官方 iLink 上游**）」。

**结论**：bridge 现在说的就是 iLink wire 协议本身，不是某种 Hub 私有抽象。Hub 是 iLink 协议的**透明中转/多路复用器**，仅在协议上叠加了 `ilink_hub_ext` 几个可选字段（`session_id` / `session_name` / `cli_session_id` / `a2a_call_id` / `a2a_depth` / `usage`）。

由此带来两个重要推论：

1. **"bridge 直连 iLink"不是架构异端，只是换 base URL。** bridge 的 iLink 客户端代码（`HubClient::{getupdates, sendmessage}`）指向真实 iLink 上游即可直连微信，无需搬迁 Hub 上游逻辑。直连唯一失去的是 Hub 的增值：多端复用、session 持久化、A2A、路由、admin 鉴权——而这些**全部承载在 `ilink_hub_ext` 这组 Hub 专有字段里**，本就不属于 iLink 协议。

2. **`ilink::types` 不是"Hub 内部类型"，而是 iLink 协议类型。** 真正属于 Hub 的只有 `HubExt`。因此抽象边界很清晰：bridge 核心说通用 IM DTO；`IlinkTransport` 负责通用 DTO ↔ iLink wire 互转；`HubExt` 这组字段是"经 Hub 中转时才有的"语义，归 Transport 的能力选项，不进通用 DTO 主干。

## 3. 核心决策：bridge 直连各 IM，Hub 退化为 iLink 专用的可选中转

基于纠正后的事实，原来的"连接放哪端"之争基本消解——bridge 本就在说 iLink 协议，直连只是换 base URL。真正要定的是下面这条。

### 决策 D：每个 IM 协议由 bridge 直连，还是经一个通用 Hub 中转？

- **D1（推荐，对齐用户方向）**：bridge 内置各 IM 协议客户端，**直连各 IM 上游**。iLink 可直连真实上游，也可仍经 Hub（换取多端复用 / session / A2A）。其他 IM（飞书 / Telegram）直接连其官方 API，**不经过 Hub**。
  - Hub 的定位收窄为「**iLink 协议的多路复用器**」——它本来就是 iLink 透明中转，不是通用 IM 网关。多端复用、session、A2A 这些增值都建立在 `ilink_hub_ext` 之上，天然只服务 iLink 这一种协议。
  - 优点：bridge 真正多 IM；新增 IM 只在 bridge 侧加一个 Transport，不动 Hub；与"iLink 只是其中一种协议"的定位完全一致。
  - 代价：直连 iLink 时失去 Hub 的 session/A2A/多端复用（除非仍走 Hub）；bridge 要自己持有各 IM 的鉴权凭证与连接生命周期。

- **D2（备选）**：把 Hub 泛化成多 IM 通用中转，bridge 仍只做 HTTP 客户端。
  - 代价大、收益小：要把 Hub 从"iLink 透明中转"重写成"多 IM 通用网关"，等于在 Hub 侧重做一遍多 IM 适配，且 `ilink_hub_ext` 这套 session 语义要推广到所有 IM——违背"Hub 是 iLink 中转"的现有事实。不推荐。

### 决策 D1（已采纳）

bridge 直连各 IM；Hub 保持 iLink 透明中转定位不变，作为 iLink Transport 的**可选后端**。配置项 `via`：

- **`via: direct`（默认，已定）**：`IlinkTransport` 直连真实 iLink 上游。direct 是更通用的形态，适用面比 hub 广，故设为默认。
- `via: hub`：仍经 Hub，换取 session/A2A/多端复用（`HubExt` 在此模式下生效）。

**进程模型（已定）**：一个 bridge 进程只服务一个 IM 渠道。各渠道上来的消息**独立启用一个 bridge 来响应，做完即关闭**——即 bridge 是按需启停的短生命周期进程，而非长驻 long-poll 守护。这使 bridge 更接近 webhook / 一次性 handler 的形态，与现有 `profile` 子命令（P0 exec 协议：读 `AGENT_*` 环境变量、写 stdout 后退出）一脉相承。

这样：
- 不需要搬迁 Hub 上游逻辑（纠正了原评估的最大风险项）；
- 多 IM 扩展点单一明确：`src/bridge/transport/` 下加一个 adapter；
- Hub 零改动即可继续服务现有 iLink 多端复用场景（`via: hub`）；
- direct 默认 + 短生命周期进程，天然适配 webhook 类 IM（飞书等），也契合"做完就关闭"的资源模型。

> 待细化（阶段设计时定，不阻塞提案）：long-poll 类 IM（iLink、Telegram）在 direct 模式下的"持续监听 + 按消息派发短任务"生命周期如何组织——是外层长驻 listener 拉取后 fork 短 bridge，还是 bridge 自身长驻。这是阶段 3 的设计细节。

## 4. 当前耦合盘点（bridge → crate 其余部分）

全模块扫描 `src/bridge/` 对 `crate::` 的引用，仅 3 处：

| 耦合点 | 引用位置 | 内容 | IM 强相关 |
|--------|----------|------|-----------|
| `crate::ilink::types` | `dispatcher/{send,session,handle,tests}.rs`、`executor.rs`、`connection.rs` | `WeixinMessage` / `SendMessageRequest` / `HubExt` / `GetUpdatesRequest/Response` / `BaseInfo` / `msg_type` | **是** |
| `crate::client` | `connection.rs` | `HubPairingClient` / `HubPairingCredentials` / `HubPairingOptions`（扫码配对） | 间接（client 又依赖 `ilink::types`） |
| `crate::paths` | `probe.rs` | `expand_user_path` + bridge 默认路径常量 | 否 |

**结论**：bridge 核心逻辑（`dispatcher` / `protocol` / `builtin` / `manager` / `config` / `probe`）本身不关心 IM，唯一把 bridge 钉死在微信上的是 `ilink::types` 这一组 wire 类型被直接用在收发路径。

## 5. 有利信号：抽离已进行了一半

`Cargo.toml` 已引入独立 crate：

```toml
agentproc = { git = "https://github.com/jeffkit/agentproc.git", rev = "dbe2b21" }
# 注释：Replaces ilink-hub's bridge/protocol.rs + executor.rs + builtin/*.rs
```

`dispatcher/agentproc_runner.rs` 已用 `agentproc::run` 驱动 turn；但 `builtin/common.rs` 仍 `use crate::bridge::protocol`，`protocol.rs` / `executor.rs` / `builtin/` 仍在。**即 AI CLI 适配层的抽离已开工但未收尾**。本提案的 IM 抽象与之正交，可并行推进。

## 6. 强约束：桌面端 Tauri 是 bridge 的强消费方

`desktop/ilink-hub-desktop/src-tauri/src/bridge_profiles.rs` 直接依赖大量 bridge 公共 API：

- `ilink_hub::bridge::BridgeApp`（`load` / `parse_yaml`）
- `ilink_hub::bridge::{probe_profile_light, dry_run_profile, ProbeError}`
- `ilink_hub::bridge::manager::{BridgeManagerOptions, spawn_bridge_manager, BridgeManagerHandle, BridgeManagerStatus}`

**任何接口抽象都必须保持这些 API 的行为兼容**（类型可换名/换路径，但签名与语义不能断崖变化），否则桌面构建直接挂。这是本提案成本最高的一块。

## 7. 目标分层模型

```
┌─────────────────────────── bridge 运行时（IM 无关）───────────────────────────┐
│                                                                                │
│  Transport trait        ← IM 协议抽象（收消息流 / 发回复 / 可选媒体上传）        │
│   ├─ IlinkTransport     ← iLink 协议客户端；via:direct（默认，直连真实上游）或     │
│  │                        via:hub（经 Hub，拿 session/A2A/复用）                    │
│   ├─ FeishuTransport    ← 预留（阶段 3+，直连飞书）                                │
│   └─ TelegramTransport  ← 预留（阶段 3+，直连 Telegram）                          │
│                                                                                │
│  通用 DTO               ← InboundMessage / OutboundReply / MediaRef            │
│                           （ilink::types 降级为 IlinkTransport 内部细节；       │
│                            HubExt 是 Hub 专有字段，不进通用 DTO 主干）           │
│                                                                                │
│  Dispatcher / Manager   ← 不变（已是 IM 无关）                                  │
│  agentproc (AI CLI)     ← 不变（独立 crate）                                    │
└────────────────────────────────────────────────────────────────────────────────┘
            │ 通用 DTO
            ▼
        AI CLI（claude / codex / …）
```

### Transport trait 草案（阶段 1 落地）

```rust
// src/bridge/transport.rs（新增）
pub trait Transport: Send + Sync {
    /// 拉取下一条入站消息（long-poll / webhook 触发，取决于实现）；
    /// 返回 TokenRejected 由上层重注册。
    async fn next_inbound(&self) -> Result<InboundOutcome>;

    /// 发送一条回复（文本 / cli_session_id 持久化）。
    async fn send_reply(&self, reply: OutboundReply) -> Result<SendOutcome>;

    /// 能力探测。阶段 1 仅 `media_upload`（可选，默认 false）；
    /// typing / 已读 / 撤回等 IM 状态能力暂不纳入（Q5 已定：暂不做 IM 状态）。
    fn capabilities(&self) -> TransportCapabilities;
}

pub struct InboundMessage {
    // 通用主干：文本 / 媒体引用 / 会话标识 / context_token
    // ...
    /// IM 适配器私有字段，承载各 IM 特有数据，避免主 DTO 膨胀（Q2 已定：需要）。
    pub extra: serde_json::Value,
}
```

`dispatcher/send.rs` 现有的 `HubClient::{getupdates, sendmessage}` + `ReplySender` trait 就是 `IlinkTransport` 的天然实现——**抽象本身是把现有代码提取到 trait 后面，不是重写**。`HubExt` 不进通用 DTO 主干，归 `IlinkTransport` 的能力选项，仅 `via: hub` 时存在（Q3 已定：选项 a）。

## 8. 阶段化路径（全部在 crate 内，不拆仓库）

### 阶段 0 — agentproc 收尾（低风险预热，独立价值）

- 删除 `src/bridge/protocol.rs` / `executor.rs` / `builtin/` 的旧实现，全面切到 `agentproc` crate。
- 验收：`cargo clippy -- -D warnings` 通过，bridge 行为不变。

### 阶段 1 — 消息层抽象（本提案核心）✅ 已落地（2026-07-17）

- 新增 `src/bridge/transport.rs`：`Transport` trait + 通用 DTO（`InboundMessage` / `OutboundReply` / `MediaRef` / `InboundOutcome` / `SendOutcome`）。trait 方法用显式生命周期 `'a` 绑定 `&self`/`buf` 并返回 `BoxFuture`，保证 object-safe + `Send`。
- 新增 `src/bridge/transport/ilink.rs`：`IlinkTransport`（`#[derive(Clone)]`），包裹现有 `HubClient` 与 `ilink::types`，**把微信类型藏在 adapter 内部**；`HubClient` / `GetUpdatesOutcome` / `parse_sendoutcome` / `classify_sendoutcome` 从 `dispatcher/send.rs` 搬入。
- 改造 `dispatcher/{send,session,handle,mod}.rs`、`executor.rs`：只依赖通用 DTO + `Transport` trait，不再 `use crate::ilink::types`。`executor::build_attachments` 移除，媒体提取收敛到 `transport::ilink::build_media`。
- `connection.rs` 归入 transport 子模块（`src/bridge/transport/connection.rs`），`bridge::` 的 5 个公共再导出（`resolve_hub_connection` / `validate_hub_token` / `default_local_credential_path` / `default_auto_client_name` / `hub_response_token_rejected`）签名不变。**`crate::client`（`HubPairingClient` / QR 渲染）不搬**——它被 Hub 服务端 `ilink/login.rs` 共享，搬进 bridge 会破坏 Hub。
- 桌面端 API（`BridgeApp` / `probe_*` / `manager::*`）**签名保持兼容**，仅内部实现切到新抽象。
- 验收：`src/bridge/` 内 `use crate::ilink::types` 计数为 0（仅 `transport/ilink.rs` 与 `transport/connection.rs` 内部允许）；`cargo fmt --check` + `cargo clippy -D warnings` + `cargo test`（738 单测 + 集成套件）全绿；`gitnexus_impact(handle_one_message)` = LOW。

### 阶段 2 — 配置化选择协议 + 可选 direct ✅ 已落地（2026-07-17）

- bridge YAML 顶层新增 `transport:` 段（默认 `ilink`）与 `via:` 段（**默认 `hub`**，避免回归现有 Hub 自动注册部署；`direct` 留待阶段 3 完成后切换），运行时按配置实例化对应 `Transport`。
- `config.rs` 新增 `TransportKind`（任意字符串，`is_ilink()` 判定）与 `Via`（`hub`/`direct`，`Deserialize` 校验未知值），挂到 `BridgeProfileFile` 与 `BridgeApp`，并暴露 `transport()` / `via()` 访问器。
- dispatcher 全程改为动态派发 `Arc<dyn Transport>`：`run_bridge_with_shutdown(transport, app, shutdown)`、`SessionDispatcher::new(client: Arc<dyn Transport>, …)`、`run_session_worker`、`handle_one_message(client: &Arc<dyn Transport>, …)`、`run_partial_forward_loop(sender: Arc<dyn Transport>, …)`、`send_final_with_retry(sender: &dyn Transport, …)`。`run_bridge(hub_url, token, app)` 保留旧签名（向后兼容桌面端），内部构造 `IlinkTransport` 包成 `Arc<dyn Transport>`。
- bin `ilink-hub-bridge.rs` 新增 `build_transport(app, cli, …)`：`ilink+hub` 走 `resolve_hub_connection` + `IlinkTransport`；`ilink+direct` 需显式 `WEIXIN_TOKEN`（用 `validate_hub_token` 探活），无则 bail 指引阶段 3；`transport: <other>` 加载 `NullTransport` 占位（每次 poll 返回 "not implemented" 让 dispatcher 退避，不忙循环）。
- `transport.rs` 新增 `NullTransport`（`#[derive(Clone)]`，实现 `Transport`），`IlinkTransport::new` 由 `pub(crate)` 改 `pub` 供 bin 构造。
- 验收：`via: hub` 行为与阶段 1 完全一致（默认值，无回归）；`via: direct` 配置可解析、显式 token 可探活、缺失 token 给出阶段 3 指引；`transport: telegram` 等可加载 `NullTransport` 证明可插拔；新增 4 个 config 单测覆盖默认值/other/direct/未知 via 拒绝。

### 阶段 3 — direct 模式凭证落地 ✅ 已落地（2026-07-20）

- 落地 `via: direct` 的 iLink 鉴权与连接生命周期。**纠正原认知**：原以为"复用 `client::HubPairingClient` 的 QR 配对"，但 `HubPairingClient` 是 Hub 下游配对（QR 编码 `ilinkhub.ai` 中继 URL、走 `/hub/register` 拿 vtoken），与直连真实上游不是一回事。真实 iLink 上游的 QR 登录是 `ilink::login::LoginClient`（`get_bot_qrcode` / `get_qrcode_status` → `bot_token`），此前只被 Hub 服务端用于引导自身 context。阶段 3 把 `LoginClient` 复用给 bridge 直连路径。
- 新增 `resolve_direct_connection(base_url, explicit_token, cred_file, force_pair, force_register, config_path)`（`src/bridge/transport/connection.rs`）：解析顺序 **显式 `WEIXIN_TOKEN` → `--pair` QR 登录真实上游 → 已存 direct 凭证文件**。无 `/hub/register`、无 `ILINK_ADMIN_TOKEN`；`validate_hub_token` 是通用 `/ilink/bot/getupdates` 探针，对真实上游同样适用。
- direct 专用凭证文件 `~/.ilink-hub/direct-credentials.json`（`paths::default_direct_credentials_path`），与 Hub 的 `bridge-credentials.json` 分离，避免 `via: hub ↔ direct` 切换互相覆盖（vtoken 与 bot_token 是不同凭证）。
- `BridgeProfileFile` 新增可选 `base_url:`（`BridgeApp::direct_base_url()`）：`via: direct` 时覆盖 `--hub-url` / `WEIXIN_BASE_URL`，使 bridge manager 可在同一 manager 下混用 hub 与 direct profile 指向不同上游。manager 子进程已传 `--config`/`--cred-file`，child 自读 YAML 的 `via:`/`base_url:`，**manager 无需改动**。
- bin `build_transport` 的 `Via::Direct` 分支接入 `resolve_direct_connection`；重连循环的 `cred_path` 与 `TokenRejected` 文案改为 via 感知（direct 失效删 direct 凭证文件后重走 QR 登录）。
- 进程模型：**不引入 ephemeral bridge 进程**。long-poll IM（iLink/Telegram）必须长驻 listener（"做完即关"与维持 `getupdates` cursor 矛盾）。当前"外层长驻 listener + 内层按消息派发短任务（session worker / agentproc turn）"即目标形态；`Transport::next_inbound` 抽象本身就是 ephemeral 的 seam——未来 webhook 型 IM 可由不同 transport 形态承载，不在阶段 3 实现。
- 已知缺口（留 seam，未实现）：direct 模式 CLI 会话续接。`via: hub` 时 Hub 回显 `session_id`（上次 `cli_session_id`）使 bridge 能 resume CLI；真实上游不回显该 HubExt 字段，direct 模式每条消息起新 CLI 会话。恢复续接需本机 store 按 `(context_token, session_name) → cli_session_id` 持久化（`handle.rs` 已留 TODO seam）。
- 验收：4 个 mockito 单测覆盖 direct 凭证解析（显式 token 接受 / 显式 token 拒绝 / 无 token QR 登录并存盘 / 已存有效 token 复用免 QR）；2 个 config 单测覆盖 `base_url:` 解析与默认 None；`paths` 单测覆盖 direct 凭证路径。`via: hub` 行为无回归。

> 注：原评估误以为阶段 3 需要"搬迁 Hub 上游逻辑"。纠正后：bridge 本就说 iLink 协议，直连只是换 base URL + 自管凭证，不搬迁 Hub。
>
> 运维注意：bridge 直连 QR 登录会从真实上游拿到一个 bot_token；若同一微信号已有 Hub 持有 bot_token，二者可能冲突。direct 模式适合"无 Hub、bridge 直接顶上"的部署，不建议与 Hub 同时登录同一微信号。

## 9. 风险与缓解

| 风险 | 等级 | 缓解 |
|------|------|------|
| `ilink::types` 抽象时 `HubExt`（session_id / a2a_call_id / a2a_depth / usage）语义漂移 | **高** | `HubExt` 是 Hub 专有、非 iLink 协议字段，归 `IlinkTransport` 的能力选项，不进通用 DTO 主干；通用 DTO 只承载 IM 协议共有的文本/媒体/会话标识；补单元测试覆盖 session/a2a 透传 |
| 桌面端 Tauri API 兼容性被破坏 | **高** | 阶段 1 保持公共导出签名不变；改路径/改名放阶段 2 且需同步改桌面端 |
| 新旧 transport 并存期出现双实现 bug | 中 | 阶段 1 完成后立即删旧路径，不留并存 |
| `client`（扫码配对）搬迁破坏 QR 流程 | 中 | **不搬 `crate::client`**：它被 Hub 服务端 `ilink/login.rs` 共享，搬进 bridge 会破坏 Hub。仅把 iLink-via-Hub 专属的 `connection.rs` 归入 `transport/`，`bridge::` 公共再导出签名不变 |
| 直连 iLink（阶段 3）凭证生命周期与现有 Hub QR 流程不一致 | 中 | **已纠正**：不复用 `HubPairingClient`（Hub 下游配对专用），改复用 `ilink::login::LoginClient`（真实上游 QR 登录）。direct 专用凭证文件与 Hub vtoken 文件分离 |
| direct 模式 CLI 会话无法续接（真实上游不回显 `session_id`） | 中 | 阶段 3 留 seam（`handle.rs` TODO）：未来加本机 `(context_token, session_name) → cli_session_id` store 恢复 resume；当前 direct 每条消息起新 CLI 会话 |
| bridge 直连与 Hub 同时登录同一微信号导致 bot_token 冲突 | 中 | 文档注明：direct 适合"无 Hub"部署，不建议与 Hub 同时登录同一微信号 |
| `paths` 中 `~/.ilink-hub` 与 `~/.ilink-hub-bridge` 路径错位 | 低 | 搬迁时逐函数对照现有常量，保留测试 `bridge_defaults_live_under_data_dir` 等 |
| 阶段 2 默认 `via` 与提案初稿（`direct`）不一致导致回归 | 中 | **已采纳 `默认 via: hub`**：阶段 2 不回归现有 Hub 自动注册部署；`direct` 留待阶段 3 完成凭证流程后切换。`run_bridge` 旧签名保留，桌面端无感 |

## 10. 改动前必做（按 CLAUDE.md）

阶段 1 动工前，对以下符号跑 `gitnexus_impact({direction: "upstream"})` 评估 blast radius，并向用户报告 HIGH/CRITICAL 风险：

- `src/bridge/dispatcher/send.rs::HubClient`（getupdates / sendmessage）
- `src/bridge/dispatcher/send.rs::ReplySender` trait
- `src/bridge/connection.rs::resolve_hub_connection`
- `src/bridge/config.rs::BridgeApp`（桌面端强依赖）
- `src/bridge/manager.rs::spawn_bridge_manager`（桌面端强依赖）

提交前跑 `gitnexus_detect_changes()` 核对受影响符号与执行流。

## 11. 关键决策（已定）

| # | 议题 | 决定 |
|---|------|------|
| Q1 | `via` 默认值 + 进程模型 | **默认 `via: direct`**（direct 比 hub 通用、适用面广）。一个 bridge 进程只服务一个 IM 渠道；各渠道消息独立启用一个 bridge 响应、做完即关闭（短生命周期进程，对齐 `profile` exec 协议形态）。不允许多 IM 共进程。 |
| Q2 | 通用 DTO 字段边界 | **需要** `extra: serde_json::Value`，承载各 IM 适配器私有字段，避免主 DTO 膨胀。 |
| Q3 | `HubExt` 归属 | **选项 a**：`HubExt` 归 `IlinkTransport` 能力选项，仅 `via: hub` 时存在；直连时 session 连续性（`cli_session_id`）退化为本机记录。不进通用 DTO 主干。 |
| Q4 | 媒体上传能力 | **可选**，`TransportCapabilities::media_upload`（默认 false）。当前媒体能力尚未启用，dispatcher 探测能力，不支持则降级。 |
| Q5 | typing / 已读 / 撤回 等状态 | **暂不做**。`TransportCapabilities` 阶段 1 仅含 `media_upload`；其余 IM 状态能力待有实际 IM 用到再加。 |
| Q6 | 与 `project-aware-backends` 叠加 | **正交**。turn 对象里 `project`（cwd，来自 `#project` 语法）与 `transport`（IM 来源）是两个独立字段，互不干涉，在 bridge 端各自解析。 |

### 阶段设计时再细化（不阻塞提案）

- long-poll 类 IM（iLink / Telegram）在 `via: direct` 下的"持续监听 + 按消息派发短任务"生命周期组织（外层长驻 listener fork 短 bridge vs bridge 自身长驻）——阶段 3 定型。
- 直连 iLink 的 session 连续性本机记录方案（替代 `HubExt` 持久化）——阶段 3 单独立提案时设计。

## 12. 参考资源

- `src/bridge/transport/ilink.rs` — `IlinkTransport` 实现（包裹 `HubClient`，藏 `ilink::types`）
- `src/bridge/transport/connection.rs` — Hub 连接解析 + 扫码配对入口（阶段 1 由 `src/bridge/connection.rs` 归入 transport 子模块）
- `src/bridge/mod.rs` — bridge 公共导出面（桌面端依赖）
- `src/ilink/types.rs` — 待抽象的微信 wire 类型集合
- `src/client/pairing.rs` — 扫码配对客户端
- `desktop/ilink-hub-desktop/src-tauri/src/bridge_profiles.rs` — 桌面端强消费方，兼容性约束来源
- `Cargo.toml:103-107` — `agentproc` crate 现状
- `docs/proposals/project-aware-backends.md` — 正交的 Backend/Project 分层提案
- `docs/knowledge/bridges/overview.md` — bridge 现有架构文档

## 13. Blast radius 量化（阶段 1 改动前评估）

### 工具结论

对 5 个关键符号跑 `gitnexus_impact({direction:"upstream"})`：

| 符号 | 工具 risk | impactedCount | 备注 |
|------|-----------|---------------|------|
| `HubClient` | LOW | 0 | **误报**：结构体实例化/方法调用未被索引 |
| `ReplySender` (trait) | LOW | 2 | d=1：`HubClient`(impl)、`ScriptedSender`(test impl) |
| `resolve_hub_connection` | LOW | 0 | **误报**：bin 实际调用未追踪 |
| `BridgeApp` | LOW | 0 | **误报**：跨 crate(desktop) + 方法调用未追踪 |
| `spawn_bridge_manager` | LOW | 5 | **可靠**：见下 |

`spawn_bridge_manager` 的可靠结果证实了桌面端调用链：

```
d=1  spawn_bridge_manager
     ├─ spawned_manager_stops_via_handle        (manager.rs 测试)
     ├─ spawned_manager_stops_when_handle_is_dropped (manager.rs 测试)
     └─ start_bridge_task                       (desktop/bridge_profiles.rs)  ← 跨 crate
d=2  ├─ bridge_start                            (desktop/bridge_profiles.rs)
     └─ bridge_restart                          (desktop/bridge_profiles.rs)
```

> **工具局限**：`gitnexus_impact` 对结构体（`HubClient` / `BridgeApp`）默认不追踪实例化与方法调用，且不跨 crate 捕获 desktop 用法，故对这两个符号返回 0 属误报。下表用 grep 补齐真实调用面。

### grep 核实的真实调用面（仅代码文件）

| 符号族 | bridge 内部 | `src/bin/ilink-hub-bridge.rs` | `desktop/.../bridge_profiles.rs` |
|--------|-------------|------------------------------|----------------------------------|
| `BridgeApp` | config(4)·manager(5)·dispatcher/{tests,session,handle,mod}(12)·mod(1) | 2 | **11** |
| `resolve_hub_connection` / `default_local_credential_path` / `validate_hub_token` | connection(6)·vtoken_env(1)·mod(3) | 4 | 0 |
| `run_bridge*` / `run_bridge_manager` / `spawn_bridge_manager` | dispatcher/mod(5)·manager(7)·mod(2) | 3 | 1 |
| `probe_profile_light` / `dry_run_profile` | probe(7)·manager(3)·mod(2) | 1 | 2 |

### 结论：blast radius 实际很收敛

1. **所有外部消费方只有两处**：CLI 二进制 `ilink-hub-bridge.rs`（同 crate，可随重构自由改动）与桌面 Tauri `bridge_profiles.rs`（**独立 crate，唯一真正的兼容性约束**）。
2. **阶段 1 的兼容性约束收敛为一条**：保持 `ilink_hub::bridge::*` 公共 re-export（`BridgeApp` / `probe_*` / `dry_run_profile` / `manager::{BridgeManagerOptions, spawn_bridge_manager, BridgeManagerHandle, BridgeManagerStatus}` / `run_bridge*`）的**签名与语义不变**。内部实现（`HubClient`、`ReplySender`、dispatcher 内部）可自由重构为 `Transport` trait。
3. **`HubExt` 语义漂移仍是唯一 HIGH 风险**（见 §9），与 blast radius 大小无关——它是协议正确性问题，不是调用面问题。
4. **阶段 1 无 HIGH/CRITICAL blast radius**：改动局限在 bridge 子模块内，外部只碰桌面端依赖的公共导出面，且该面可保持不变。

> 按 CLAUDE.md，阶段 1 实际动工时仍需对每个被改函数重跑 `gitnexus_impact` 并在提交前跑 `gitnexus_detect_changes()`。本节为提案阶段的事前评估。

## 14. 阶段 0 调研结论与后续独立提案

### 阶段 0 调研结论（2026-07-16）

实施前实测 agentproc 迁移现状，发现提案阶段 0 的前提部分有误：

- **`protocol.rs`**：确为 agentproc 已导出类型（`Attachment`/`TurnObject`/`TurnInput`/`PROTOCOL_VERSION`）的纯重复定义，`TurnObject::new` 6 参数签名逐字一致；`agentproc_runner.rs:73` 的 `to_agentproc_attachments` 是冗余转换 shim。**可去重，机械操作。**
- **`executor.rs`**：早已削减为 3 个 ilink-hub 专属 helper（`build_attachments` / `split_into_parts` / `MAX_CLI_CAPTURE_BYTES`），是活代码，不属于 agentproc，**不能删。**
- **`builtin/`**：agentproc `run()` 在未启用 `executors` feature 时**永远走 spawn 路径**（`agentproc runner.rs:160-161`），完全忽略 `executor:` 字段；ilink-hub Cargo.toml 未启用该 feature。`builtin/` 是 `ilink-hub-bridge profile <type>` 子命令的 spawn 目标实现，**正在用，与 agentproc executors feature 不重复，不能删。**

**决定**：跳过阶段 0（其唯一成立项 `protocol.rs` 去重对阶段 1 无依赖、价值边际），直接进入阶段 1。

### 后续独立提案（待立）：`executor:` 字段 in-process 化

副发现：`BridgeProfile.executor` 字段当前在 ilink-hub 构建里是**死字段**——agentproc 未启用 `executors` feature，`run()` 永远 spawn，`executor:` 被忽略。若期望 `executor: claude-code` 走 in-process（跳过 bridge 子进程），需：

1. Cargo.toml 启用 `agentproc` 的 `executors` feature；
2. 将 `src/bridge/builtin/` 的 executor 实现迁移到 agentproc 的 `Executor`/`TurnHandlers` 框架，或用 agentproc 自带的 executor 集合；
3. 验证 `executor:` + known 走 in-process、unknown 回退 spawn 的四象限行为。

此为独立重构，单独立提案推进，不在本提案范围。
