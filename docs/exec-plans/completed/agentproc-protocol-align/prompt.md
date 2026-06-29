# agentproc 协议对齐（Step 1：Rust bridge 协议改名）

## 背景

ilink-hub 的 bridge 协议（P0）已抽出为独立项目 `agentproc`，并演进到 v0.3.0：
env 变量从 `ILINK_*` 改名为 `AGENT_*`，sentinel 行从 `ILINK_PARTIAL:`/`ILINK_SESSION:`
改名为 `AGENT_PARTIAL:`/`AGENT_SESSION:`，并新增 `AGENT_ERROR:`、`AGENT_PROTOCOL_VERSION`。

ilink-hub 需要反向对齐 agentproc v0.3.0，使后续能用 agentproc 的官方 SDK/profile
替代 ilink-hub 自带的 `sdk/`。本次只做 Step 1：Rust 侧协议改名 + 最小新契约补齐。

## 目标（用户视角）

作为 ilink-hub 的 bridge 运行时，我能按照 agentproc v0.3.0 协议与 profile 进程通信，
使任何遵循 agentproc 协议的 profile（包括未来用 `agentproc` SDK 写的）都能直接接入 Hub。

## 完成标准

1. `cargo fmt --all -- --check` 通过
2. `cargo clippy -- -D warnings` 零 warning
3. `cargo test` 全部通过（含 bridge 模块现有测试）
4. `src/bridge/` 下不再出现 `ILINK_MESSAGE` / `ILINK_SESSION_ID` / `ILINK_SESSION_NAME` /
   `ILINK_FROM_USER` / `ILINK_CONTEXT_TOKEN` / `ILINK_STREAMING` / `ILINK_PARTIAL` /
   `ILINK_SESSION:` 字面量（全部改为 `AGENT_*`）
5. Rust executor 向 profile 进程注入 `AGENT_PROTOCOL_VERSION` 环境变量（值取自常量，对齐 agentproc v0.3.0）
6. Rust executor/dispatcher 能解析 `AGENT_PARTIAL:` 与 `AGENT_SESSION:` 行，行为与原 `ILINK_*` 一致
7. 至少一个内置 profile（`builtin/echo` 或现有 builtin 的 dry-run/probe）端到端跑通：
   注入 `AGENT_MESSAGE` → 收到 `AGENT_PARTIAL:` 流式 → 收到最终 body

## 非目标

- 不删除/不改 `sdk/python`、`sdk/node`（Step 2 处理）
- 不改 `examples/`、`docs/` 里的包名/import（Step 2/3 处理）
- 不实现 agentproc 的 `AGENT_ATTACHMENTS` / stdin EOF 新契约（留待后续 step）
- 不重写 builtin profile 为 agentproc profile 脚本（Step 4 决策）
- 不改 `ILINK_CONTEXT_TOKEN` 之外的其他 ilink-hub 专有 env（如 hub context token 机制本身）

## 硬约束

- 协议名映射严格按 agentproc v0.3.0 spec：
  - `ILINK_MESSAGE` → `AGENT_MESSAGE`
  - `ILINK_SESSION_ID` → `AGENT_SESSION_ID`
  - `ILINK_SESSION_NAME` → `AGENT_SESSION_NAME`
  - `ILINK_FROM_USER` → `AGENT_FROM_USER`
  - `ILINK_STREAMING` → `AGENT_STREAMING`
  - `ILINK_PARTIAL:` → `AGENT_PARTIAL:`
  - `ILINK_SESSION:` → `AGENT_SESSION:`
  - 新增 `AGENT_PROTOCOL_VERSION`（注入）、`AGENT_ERROR:`（解析，按 spec 转发为错误回复）
- `ILINK_CONTEXT_TOKEN`：agentproc spec 无此变量，属 ilink-hub 专有。**保留原名**
  （这是 Hub 内部 context 机制，不属于 agentproc 协议范畴）。在 plan.md 中显式列出此例外。
- 生产路径禁止裸 `unwrap()`
- 不在 main 分支提交，走 worktree 隔离
