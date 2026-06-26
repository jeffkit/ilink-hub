# plan.md — agentproc 协议对齐 Step 1（Rust bridge 协议改名）

## 架构

单里程碑。改动集中在 `src/bridge/`，分 5 个改动簇：

```
profile 进程  ──env AGENT_*──▶  executor.rs (注入 + 解析 AGENT_PARTIAL:)
                                      │
                                      ├── config.rs (cli_session_first_line_prefix 默认值)
                                      ├── probe.rs (dry-run env 注入)
                                      ├── builtin/common.rs (P0 helper：emit/parse)
                                      └── builtin/{cursor,codex,claude_code,codebuddy,agy,mod}.rs (注释 + 输出)
```

## 协议名映射表（严格按 agentproc v0.3.0 spec）

| 旧 | 新 | 处理 |
|----|----|------|
| `ILINK_MESSAGE` | `AGENT_MESSAGE` | 改 |
| `ILINK_SESSION_ID` | `AGENT_SESSION_ID` | 改 |
| `ILINK_SESSION_NAME` | `AGENT_SESSION_NAME` | 改 |
| `ILINK_FROM_USER` | `AGENT_FROM_USER` | 改 |
| `ILINK_STREAMING` | `AGENT_STREAMING` | 改 |
| `ILINK_PARTIAL:` | `AGENT_PARTIAL:` | 改 |
| `ILINK_SESSION:` | `AGENT_SESSION:` | 改（config.rs:509 默认值 + 各处字面量） |
| `AGENT_PROTOCOL_VERSION` | (新增注入) | 新增常量 `AGENTPROC_PROTOCOL_VERSION = "0.3"`，executor 注入 |
| `AGENT_ERROR:` | (新增解析) | executor 解析为错误回复（按 spec：partial 已发则不重复，发错误回复） |

## 保留原名（例外，**不改**）

以下属 ilink-hub 自有机制，不在 agentproc 协议范畴：

- `ILINK_CONTEXT_TOKEN` — Hub 内部 context token，agentproc spec 无此变量
- `ILINK_ITEM_TYPE` / `ILINK_IMAGE_URL` / `ILINK_FILE_URL` / `ILINK_FILE_NAME` / `ILINK_VIDEO_URL`
  — ilink-hub 自定义附件契约（agentproc `AGENT_ATTACHMENTS` 仍为 draft，留待后续 step）
- 所有 `ILINK_HUB_*` / `ILINK_ADMIN_*` / `ILINK_CORS_*` / `ILINK_TOKEN` / `ILINK_BASE_URL` /
  `ILINK_MAX_*` / `ILINK_QUEUE_*` / `ILINK_SHUTDOWN_*` / `ILINK_DISPATCH_*` / `ILINK_QUOTE_*` /
  `ILINK_*_MODEL` / `ILINK_APP_ID` 等 — Hub 自身配置/常量，与 bridge 协议无关
- `ILINK_BASE_URL` / `ILINK_CDN_BASE_URL` 常量、`iLink-App-Id` HTTP header

> 判定原则：**仅当变量是 bridge↔profile 进程之间的 P0 通信契约时才改名**。Hub 自身运行配置保留。

## 里程碑 M1：Rust bridge 协议改名 + 最小新契约

### 改动清单

1. `src/bridge/executor.rs`
   - L218-238：env 注入改名（5 个变量），新增 `AGENT_PROTOCOL_VERSION` 注入
   - L322：`strip_prefix("ILINK_PARTIAL:")` → `"AGENT_PARTIAL:"`，新增 `AGENT_ERROR:` 解析分支
   - L329：warn 文案 `ILINK_PARTIAL` → `AGENT_PARTIAL`
   - L478/485：测试字面量改 `AGENT_SESSION:`
   - L146-170 注释（如有提及 P0 协议名）按需更新

2. `src/bridge/config.rs`
   - L110-111：doc 注释 `ILINK_PARTIAL:` → `AGENT_PARTIAL:`
   - L509：`"ILINK_SESSION:"` → `"AGENT_SESSION:"`

3. `src/bridge/probe.rs` L199-203：5 个 env 改名（`ILINK_CONTEXT_TOKEN` 保留）

4. `src/bridge/builtin/common.rs`
   - L4-6/20-21/54-59/64-67：env 读取 + `ILINK_SESSION:`/`ILINK_PARTIAL:` 输出改名

5. `src/bridge/builtin/{cursor,codex,claude_code,codebuddy,agy,mod}.rs`
   - 所有注释中 `ILINK_PARTIAL:`/`ILINK_SESSION:` → `AGENT_*:`
   - 如有 `println!("ILINK_SESSION:...")` / `ILINK_PARTIAL:` 输出字面量 → 改名

6. `src/bridge/dispatcher.rs` L1039/1107：注释/warn 文案 `ILINK_PARTIAL` → `AGENT_PARTIAL`

7. 新增常量：在 `src/bridge/mod.rs` 或 `executor.rs` 顶部定义
   `pub const AGENTPROC_PROTOCOL_VERSION: &str = "0.3";`

### 验证命令

```bash
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -p ilink-hub bridge::   # bridge 模块单测
cargo test                           # 全量（含 store_tests 等预存）
# 残留检查（应只剩例外清单中的变量）：
rg -n 'ILINK_(MESSAGE|SESSION_ID|SESSION_NAME|FROM_USER|STREAMING|PARTIAL|SESSION:)' src/bridge/ && echo "RESIDUAL FOUND" || echo "CLEAN"
```

### 端到端验证

用 `probe.rs` 的 `dry_run_profile` 或现有 echo builtin 做一次端到端：
注入 `AGENT_MESSAGE="hi"` → 期望收到 `AGENT_PARTIAL:` 流式 + 最终 body。
若仓库无 echo builtin，用 `cargo test bridge::executor::tests` 覆盖解析路径即可。

### E2E checkpoint

**E2E checkpoint：** not-ready
**E2E 判定依据：** e2e-protocol Step B — 本里程碑仅改动 bridge 内部协议名映射，不产出新的可测试外部接口（无新 API endpoint / UI 入口）；协议行为通过单测 + probe dry-run 验证。完整 E2E（真实 profile 进程接入）留待 Step 2 SDK 替换后。
**E2E 场景：** N/A（not-ready）
**Visual Review：** not-needed

### 风险

- builtin profile 的 stdout 协议必须与 executor 解析同步改名，否则流式断链 → 用单测 + grep 双重保险
- `cli_session_first_line_prefix` 是配置字段，用户自定义 YAML 里可能硬编码了 `ILINK_SESSION:` → 在 CHANGELOG/journal 标注 breaking change，旧 YAML 需更新
- `store_tests.rs` 等预存测试与本次无关，若 `cargo test` 预存失败需在 PR body 声明
