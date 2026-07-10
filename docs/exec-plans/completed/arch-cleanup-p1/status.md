# arch-cleanup-p1 — Status

更新时间：2026-07-09

## 当前进度

**已归档（superseded / completed-by-main）**。本计划分支未完成执行，但 N-01..N-07 对应改动已在 main 落地，不再按本 plan 重做。

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1 可靠性修复（N-01 + N-04） | ✅ superseded | `last_seen` DashMap 清理 + Mutex poison 一致性已在 main |
| M2 可观测性精度（N-02） | ✅ superseded | `LatencyHistogram` 微秒精度已在 main |
| M3 低风险修复 + rand 升级（N-03 + N-05） | ✅ superseded | `column_exists` 驱动感知 + rand 0.9 已在 main |
| M4 HubError 具体化（N-06） | ✅ superseded | `UpstreamHttp` / `UpstreamParse` 已在 `src/error.rs` |
| M5 handle_hub_command 拆解（N-07） | ✅ superseded | `handle_hub_command` 已在 `hub/commands.rs` 并有单测 |

## 归档说明

- 工作在 plan 分支外合入 main；本目录仅保留历史四件套供追溯。
- **不要**再按本 plan 重新实现 N-01..N-07。
- 由 `security-hardening-abc` M4 文档债清理时移入 `docs/exec-plans/completed/`。

## 原环境（历史）

- 分支：`refactor/arch-cleanup-p1`（未作为交付路径）
- Exec-plan：原 `docs/exec-plans/active/arch-cleanup-p1/`
