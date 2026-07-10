# security-hardening-abc — Plan

> 基于：prompt.md

## 架构概览

```
M1 安全 Critical ──► M2 安全 High ──► M3 God 拆分 ──► M4 文档/归档
   CORS+vtoken          shell/log/loopback   dispatcher+desktop   queue说明+plans
```

## M1：CORS 接线 + vtoken 单次领取

- `src/server/mod.rs`：`bot_cors = build_cors_layer()` 替代 `CorsLayer::permissive()`
- `tests/cors_tests.rs`：增加 `build_router` 级断言（设置白名单时 evil Origin 无 ACAO 或非 `*`）
- `PairingRegistry`：新增 `take_confirmed_vtoken(code) -> Option<String>`（或等价）：首次 status 读 confirmed 时取出并清除 vtoken（或 remove confirmed 后保留无 token 的 stub）；再次 get 返回 confirmed 但 `bot_token=None`
- 兼容合法客户端：首次 poll 仍能拿到 token；二次 poll 不报错但无 token
- 缩短或保留 CONFIRMED_TTL 均可，但单次领取是硬要求

**验收：** `cargo test --test cors_tests` + pairing 相关单测 + `cargo test pairing -- --test-threads=1`
**E2E 覆盖：** not-needed  
**E2E 判定依据：** e2e-protocol Step B「纯库/daemon 内部安全修复、无独立 HTTP E2E 框架」→ 本仓 E2E capable=false（见 arch-cleanup status / AGENTS）

## M2：shell 硬拒绝 + 日志脱敏 + 桌面 loopback

- `warn_shell_injection_risk` → 改为返回 `Result` / 在 load 路径 `bail!`
- `serve.rs`：日志只打 scheme/host/db，密码 redact
- desktop `resolve_initial_listen_addr`：解析 `ILINK_HUB_ADDR` 后强制 host ∈ {127.0.0.1, localhost, ::1}，否则 Err

**验收：** 对应单元测试 + `cargo test` 相关模块 + desktop `cargo test --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml`
**E2E 覆盖：** not-needed（同上，daemon/桌面单元可测）

## M3：God 模块拆分（至少 2 个）

优先顺序：
1. `src/bridge/dispatcher.rs` → 抽出 `retry` / `stream` / `session_worker` 等子模块（同目录 `dispatcher/`）
2. `desktop/.../src-tauri/src/lib.rs` → 抽出 `listen_addr` / `hub_controller` / `commands` 等

约束：行为不变；拆分后各文件 < ~1200 行优先；全量测试通过。

**验收：** `wc -l` 原文件显著下降；`cargo test` + desktop check
**E2E 覆盖：** not-needed（纯重构，无行为变更意图）

## M4：文档债 + 队列产品限制 + 归档

- 更新 `docs/knowledge/project/overview.md`、`configuration.md`（bind 默认 127.0.0.1；队列 memory）
- 部署加固文档补充「队列易失」
- 归档/修正过期 active plans（`arch-cleanup-p1` status、已完成的 mutation/desktop plans）

**验收：** 文档与代码一致；`docs/exec-plans/active/` 噪音下降
**E2E 覆盖：** not-needed（文档）
