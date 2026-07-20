# Code Review：`refactor/bridge-transport-abstraction`

| 项 | 值 |
|---|---|
| 分支 | `refactor/bridge-transport-abstraction` |
| Worktree | `.worktrees/refactor-bridge-transport-abstraction` |
| 对比基线 | `main` (`1f4cb24`) |
| Commits | `3c8e861` stages 1–2 · `777691d` stage 3 · `a6a3863` review 加固 |
| 审查日期 | 2026-07-20（初审） / 2026-07-20（复审） |
| 审查范围 | `src/bridge/transport*`、`config`、`dispatcher/*`、`executor`、`bin/ilink-hub-bridge`、`paths`、`manager`、提案 |
| 单测快照 | 初审 219 passed；复审 **223 passed / 0 failed / 1 ignored**，clippy `-D warnings` 干净 |
| 总体风险 | **低**（复审后）— 上轮 H1/M2/M3/M5 主路径已闭环；残留 N1（运行时 TokenRejected→子进程内 QR）建议合入前补 |

---

## 1. 一句话结论

这是一次**边界清晰、行为保持型**的重构：`Transport` trait + 通用 DTO 把 `crate::ilink::types` 成功收敛进 `transport/ilink.rs`，`via: hub` 语义与重构前一致。主要问题不在抽象本身，而在 **stage 3 `via: direct` 的运维闭环未闭合**（manager 非交互引导、缺 `base_url` 静默回退），以及**提案关键决策与实现不一致**。

**合入建议**：可以合入，但合入前建议至少处理 H1 + M2 + M3（见下）。

---

## 2. 改动概览

| 区域 | 变化 |
|------|------|
| `src/bridge/transport.rs` | 新增 `Transport` / DTO / `NullTransport` |
| `src/bridge/transport/ilink.rs` | `IlinkTransport` + 原 `HubClient` 下沉 |
| `src/bridge/transport/connection.rs` | Hub 连接解析迁入 + `resolve_direct_connection` |
| `src/bridge/dispatcher/*` | 全程 `Arc<dyn Transport>`，去掉对 wire 类型依赖 |
| `src/bridge/config.rs` | `transport:` / `via:` / `base_url:` |
| `src/bin/ilink-hub-bridge.rs` | `build_transport`；reconnect 感知 via |
| `src/paths.rs` | direct 凭证路径 |
| 提案文档 | stages 1–3 标记已落地 |

公共导出面（桌面端依赖的 `BridgeApp` / `run_bridge*` / `spawn_bridge_manager` 等）基本保持；`run_bridge(hub_url, token, app)` 旧签名保留。

---

## 3. Findings

### 🔴 High

#### H1 — Manager 模式下 `via: direct` 无法非交互完成凭证引导，失败会重启风暴

**证据**

- `bridge_child_args`（`src/bridge/manager.rs:927-942`）只传 `--hub-url/--cred-file/--register-name/--config`，**不传 token**。
- `spawn_bridge_child`（`manager.rs:951`）显式 `env_remove("WEIXIN_TOKEN")`。
- 因此 child 走 `resolve_direct_connection` 时，无显式 token → 落到 `qr_login_and_save_direct` → `LoginClient::login_with_qr()`（终端打印二维码并阻塞轮询，最长约 30 分钟）。
- 子进程 stdio 继承 manager；多 profile 并发时二维码会互相穿插。QR 超时/失败 → child 退出 → manager backoff 重启 → **再次 QR → 循环**。

**为什么重要**

提案 §8 stage 3 写「manager 无需改动，child 自读 `via:`/`base_url:`」，但**凭证引导在 headless supervisor 下走不通**——属于「配置可写、运行不可用」的能力缺口，失败模式是重启风暴而非 fail-fast。

**建议**

1. Manager 发现 profile `via: direct` 时：启动前校验对应 cred 文件可用（或存在有效 direct token），否则 **拒绝 spawn** 并打清晰日志（指引先手动 `ilink-hub-bridge --config … --pair`）。
2. 或在文档/配置层明确：**manager 暂不支持 `via: direct`**，加载时直接报错。
3. 补 manager + `via: direct` 的单元/集成测试锁住该守卫。

---

### 🟠 Medium

#### M2 — `via: direct` 缺 `base_url:` 时静默回退到 Hub / localhost URL

**证据**（`src/bin/ilink-hub-bridge.rs:190-193`）

```rust
let base = app
    .direct_base_url()
    .map(str::to_string)
    .unwrap_or_else(|| cli.hub_url.trim().trim_end_matches('/').to_string());
```

`--hub-url` 默认 `http://127.0.0.1:8765`。用户写了 `via: direct` 但漏配 `base_url:` / `WEIXIN_BASE_URL` 时，会对 **Hub 或 localhost** 发起 `get_bot_qrcode`，语义完全错误且无 WARN。

在 manager 场景下更危险：child 必带 manager 的 `--hub-url`，缺 `base_url:` 的 direct profile **必然**把 Hub 当成「真实上游」。

**建议**

- `Via::Direct`：要求显式 `base_url:` 或非默认的 `WEIXIN_BASE_URL`；否则 bail。
- 至少：当 base 像本地 Hub 默认地址时 `tracing::warn!` / fail-fast。
- 为 `build_transport` 的该分支补单测。

#### M3 — 提案与实现的默认 `via` 自相矛盾

**证据**

| 来源 | 默认值 |
|------|--------|
| 代码 `Via`（`config.rs:46-51`） | **`Hub`**（`#[default]`） |
| 提案 §3 D1、§11 Q1 | **`direct`** |
| 提案 §8 stage 2 / §9 风险表 | 改口为已采纳默认 **`hub`** |

代码选 hub（避免现网回归）是正确的；但提案 Q1/§3 未同步，后续实现者可能按 Q1 「纠正」默认值造成回归。

**建议**：统一提案：Q1 / §3 改为「默认 `via: hub`；`direct` 为显式 opt-in」，并注明与初稿 D1 的差异及原因。

#### M4 — 流式 partial 传输失败 + final 去重 → 可能静默丢回复尾部

**证据**

- `send.rs:181-190`：partial 遇 `Err` 直接 `pending = None`（不重试），与 `send_final_with_retry` 对传输错误重试的策略不对称。
- `handle.rs:176-182`：`body_already_delivered = streaming && partial_count > 0`，然后 final body 置空。
- `partial_count` 统计的是 CLI **产出**分片数（`agentproc_runner` 回调），不是**成功送达**数。

若最后一个 partial 因传输错误被丢弃，仍可能 `partial_count > 0` → final 被跳过 → 用户看不到尾部，且无用户可见错误。

**说明**：该逻辑大概率是重构前既有行为被原样保留，故标 Medium 而非阻塞。建议后续用「成功发送计数」驱动去重，或 partial 失败时禁止抑制 final。

#### M5 — Manager 共用 `--cred-file` 目录，hub↔direct 切换可能踩旧凭证

Manager 始终把 `--cred-file` 指到 `credentials_dir/{id}.json`（hub vtoken 与 direct bot_token **同路径族**）。profile 从 `via: hub` 改成 `via: direct`（或相反）时：

- 旧文件可能被当成「存在但不可用」或「token 被拒 → 删除再 QR」；
- 与提案强调的「`direct-credentials.json` 与 `bridge-credentials.json` 分离」在 **CLI 默认路径**上成立，在 **manager 托管路径**上不成立。

**建议**：manager 按 via 分目录或文件后缀（如 `{id}.direct.json`）；切换 via 时 fail-fast 提示清理旧凭据。

#### M6 — `TransportCapabilities` 未接入 dispatcher（Q4 未落地）

提案 Q4：dispatcher 应探测 `media_upload`，不支持则降级。现状：`capabilities()` 仅在 trait / mock 中实现，**dispatcher 零调用**。当前媒体出站本就未启用，暂无功能回归，但 seam 是死的——后续加媒体时容易忘记接线。

**建议**：哪怕只加 `debug_assert` / 启动日志打印 capabilities，或在 send 路径对 `media_upload == false` 明确跳过。

---

### 🟡 Low

#### L1 — `InboundMessage.extra` 是死 seam（相对 Q2）

提案 Q2 要求 `extra` 承载 IM 私有字段；`weixin_to_inbound`（`ilink.rs:283`）恒置 `extra: Null`，字段带 `#[allow(dead_code)]`。属预留缺口，非 bug。若调试依赖旧的 item.extra dump，行为已弱化（现只 dump `raw`）。

#### L2 — 死代码 / 双解析

- `classify_sendoutcome`（`ilink.rs:35-36`）全程 `#[allow(dead_code)]`，且与 `parse_sendoutcome` 语义不一致（前者把非 -2 的 ret 当 Sent，后者当 Err）。
- `HubClient::sendmessage` 为打 warn 对同一 body 二次 `serde_json::from_str`（`ilink.rs:165-175`）。

#### L3 — 日志文案仍写 “hub”，direct 路径易误导

`getupdates` TokenRejected 日志：`hub rejected virtual token`（`ilink.rs:128`）。direct 模式下运维会搜错关键字。TokenRejected bail 文案仍提到 `ilink-hub register`（`ilink-hub-bridge.rs:386-388`），对 direct 不完全准确。

#### L4 — `NullTransport` 会让进程永久 backoff，不会 fail-fast

未知 `transport:` 加载占位后，`next_inbound` 每次 Err → dispatcher 3→60s 退避循环。对「可插拔证明」够用；若误配进 manager，会变成**假活僵尸进程**。建议启动时对非 ilink transport 直接 bail（或仅 `--allow-null-transport` 开启占位）。

#### L5 — `config.rs` 注释过期

`Via` 上注释仍写「Stage 2 only supports `direct` with explicit `WEIXIN_TOKEN`… stage 3」（约 43–45 行），与 stage 3 已落地 QR 矛盾，易误导读者。

#### L6 — direct 模式无 CLI resume（已知 seam）

`handle.rs:102-110` TODO 已文档化：真实上游不回显 `HubExt.session_id`，每条消息新 CLI 会话。建议启动 `via: direct` 时打一条 INFO，避免用户以为「切 direct 行为完全等价」。

#### L7 — `ILINKHUB_BRIDGE_DUMP_MSG` 仍会把完整 `raw` 打到 stderr

既有调试开关；生产勿开。非本 PR 引入。

---

## 4. 与提案的差距（Gaps）

| 提案承诺 | 实现现状 | 评级 |
|----------|----------|------|
| 默认 `via: direct`（Q1/§3） | 默认 `hub`；§8/§9 另有说法 | M3 文档债 |
| Manager 无需改动即可混用 hub/direct | 凭证引导与 base_url 回退未闭环 | H1 / M2 / M5 |
| Q2 `extra` 承载私有字段 | 字段存在，恒 Null | L1 |
| Q4 capabilities 探测 | trait 有、dispatcher 未用 | M6 |
| direct CLI resume 本机 store | 明确未做（TODO） | L6（已知） |
| 短生命周期「做完即关」进程模型（Q1） | stage 3 纠正为长驻 listener；合理 | 文档需与 Q1 对齐 |

---

## 5. 测试覆盖缺口

| 缺口 | 优先级 |
|------|--------|
| Manager + `via: direct`（H1）无任何测试 | **P0** |
| `build_transport` hub/direct/null 选择与 base_url 回退（M2） | **P0** |
| `NullTransport`「Err → 退避不忙等」dispatcher 级测试 | P1 |
| direct `--pair` / `ExistsUnusable` + `force_register` | P1 |
| saved token 有效但 `base_url` 变更 → 重写凭证（`connection.rs:541-545`） | P2 |
| 集成级：direct **不调用** `/hub/register` | P2 |
| `capabilities()` 被消费后的行为测试 | P2（待 M6） |

已有亮点：`resolve_direct_connection` 的 4 个 mockito 单测、config 默认/非法 via、凭证路径分离 —— stage 3 核心解析路径覆盖尚可。

---

## 6. 做得好的地方

1. **抽象边界干净**：`Transport` + `BoxFuture` object-safe；dispatcher 不再 `use crate::ilink::types`；`HubExt` 正确留在 iLink adapter。
2. **安全细节到位**：凭证文件权限、token `trim()`、hub/direct 默认路径分离、`ExistsUnusable` 不静默覆盖、shell 注入守卫保留。
3. **并发与生命周期**：partial/final 重试环的 cancel-safety、节流预算、`sanitize_*`、mutex 中毒恢复、session worker 上限 + 清理 —— 迁移后仍扎实。
4. **向后兼容**：旧 `run_bridge` 签名、桌面公共导出面未破坏；默认 `via: hub` 避免现网回归。
5. **测试质量**：backoff、partial 不变量、`parse_sendoutcome` 对抗样本、QR 变异测试等保留并扩展。

---

## 7. 合入前检查清单（初审）

- [x] **H1**：manager 对 `via: direct` fail-fast 或预检凭证（主路径已修；见复审 N1）
- [x] **M2**：direct 强制/校验 `base_url`（禁止静默打 localhost Hub）
- [x] **M3**：提案 Q1/§3/进程模型 与实现、§8 stage 3 叙述对齐
- [x] **M5**：manager cred 路径按 via 隔离（防覆盖已修；清理表述见复审 N2）
- [x] 启动 direct 时 INFO 提示无 CLI resume（L6）
- [x] 刷新过期注释（L5）与 TokenRejected 文案（L3）
- [x] `cargo clippy -D warnings` / `cargo test … bridge::`（复审时绿）

---

## 8. 审查方法说明

- 通读分支相对 `main` 的全部 diff 与关键实现文件。
- 对照提案 `bridge-as-multi-im-runtime.md` 的阶段验收与关键决策表。
- 交叉验证 manager spawn、direct 凭证解析、partial/final 去重路径。
- GitNexus：当前索引锚定主工作区 `main`，**未包含本 worktree 新符号**（`detect_changes(compare main)` / `impact(resolve_direct_connection)` 不可用）；合入后建议 `npx gitnexus analyze` 再跑一遍影响面。

---

## 9. 审查人备注（初审）

净判断：**架构方向正确，hub 路径可放心；direct 路径差「运维闭环」最后一公里。** 优先修 H1/M2，避免用户在 manager 或漏配 `base_url` 时踩坑；M3 文档对齐可防止后续误改默认值。

---

## 10. 复审（2026-07-20，`a6a3863`）

针对提交 `a6a3863 fix(bridge): close via: direct ops loop (review H1/M2/M5 + hardening)` 的二次审查。

### 10.1 上轮 findings 状态

| 编号 | 状态 | 证据 |
|------|------|------|
| **H1** | **Partial** | `direct_spec_needs_credentials`（`manager.rs:1048`）；spawn（`:527-546`）与 restart（`:451-463`）均拒绝无凭证；测试 `start_new_children_refuses_direct_without_credentials`。**运行时撤销路径未堵 → 见 N1** |
| **M2** | **Fixed** | `resolve_direct_base_url`（`ilink-hub-bridge.rs:168-187`）拒绝默认 localhost；多测覆盖 |
| **M3** | **Fixed** | 提案 §3 / §11 Q1 改为默认 `via: hub`，并注明与初稿差异 |
| **M4** | **Deferred** | 提案 §8 明确记为既有非阻塞项，本轮未改 |
| **M5** | **Fixed（防覆盖）** | `cred_filename` → `{id}.direct.json`；「via 切换清理」表述夸大 → 见 N2 |
| **M6** | **Addressed（最低限度）** | direct 启动 `info!` 打 capabilities；hub 分支未打 → 见 N4 |
| **L2** | **Mostly Fixed** | 删除 `classify_sendoutcome`；二次 JSON parse 仍在（可接受） |
| **L3–L6** | **Fixed** | via 感知文案 / `--allow-null-transport` fail-fast / 注释刷新 / resume INFO |
| **L1 / L7** | **Open/Deferred** | 符合预期，非阻塞 |

### 10.2 复审新发现

#### 🟠 N1 — H1 只堵了 spawn/restart；运行时 TokenRejected 仍会在子进程内 QR

Manager 守卫在**父进程**。已 Running 的 direct 子进程若上游运行时撤销 token：

1. `BridgeStop::TokenRejected` 且非 explicit token（manager 从不传 token）→ **删 cred_file** + `continue 'reconnect'`（`ilink-hub-bridge.rs:456-468`）
2. 重连 → `resolve_direct_connection` 见 Missing → `qr_login_and_save_direct` → `login_with_qr()` 在 headless 子进程阻塞 ~30min

上轮 H1 想消掉的失败模式换了触发点仍可达。Manager 只有等子进程最终退出后才会 park。

**建议**：`qr_login_and_save_direct`（或 `resolve_direct_connection`）在非交互环境（非 TTY / `--no-interactive` / manager 注入的 env）直接 bail，把控制交回 manager 的凭证守卫；勿在 headless 下发起 QR。

#### 🟡 N2 — M5「切换时清理旧凭证」是提案/提交信息夸大

`stop_removed_or_changed`（`manager.rs:294+`）仅在 profile **删除**时 `remove_file`；fingerprint **变更**（含 hub↔direct）只停子进程、不删旧 cred。hub→direct 后旧 `{id}.json` 会成孤儿文件；hub client 也可能未 deregister（changed 不走注销）。功能上不覆盖（文件名已隔离），但提案 §8/§9 写「via 切换随 fingerprint 清理」不实。

**建议**：修正文档表述，或在 via 翻转时清理旧后缀文件 + 按需 deregister。

#### 🟡 N3 — Manager 守卫只查凭证，不预检 base_url

有可用 cred 但缺 `base_url:` 且 manager `--hub-url` 仍是默认时：过 H1 守卫 → 子进程命中 M2 bail → 退出 → manager 有界退避重启循环。有清晰日志，不是紧密风暴，但仍可避免。

**建议**：manager 对 `via_direct` 同时预检 YAML `base_url:` 非空（或可解析），与缺凭证一样 park。

#### ⚪ N4 — capabilities 日志仅 Direct 分支

`build_transport` 只在 `Via::Direct` 打印 capabilities（`:263-268`），默认 hub 路径不打。把日志移到两分支公共出口即可。

### 10.3 复审结论

- 修复提交质量高：上轮合入阻塞项基本落地，测试与提案同步扎实。
- 净风险：**中低 → 低**。
- **合入建议：可以合入**；强烈建议合入前补 **N1**（成本低：非交互 fail-fast）。N2–N4 可 follow-up。

### 10.4 复审后合入清单

- [ ] **N1（建议必做）**：direct QR / 重连在非交互环境下 fail-fast
- [ ] **N2**：修正「via 切换清理旧凭证」表述，或真实现清理
- [ ] **N3**：manager 对 direct 预检 `base_url`
- [ ] **N4**：capabilities 日志覆盖 hub 分支
