# arch-cleanup-p1 — Status

更新时间：2026-06-21

## 当前进度

Phase 1-2 完成（prompt.md + plan.md）。等待 flowx force-dev 启动 Phase 3 执行。

## 里程碑

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1 可靠性修复（N-01 + N-04） | ⏳ 待执行 | last_seen 注销清理 + Mutex poison 一致性 |
| M2 可观测性精度（N-02） | ⏳ 待执行 | LatencyHistogram 微秒精度 |
| M3 低风险修复 + rand 升级（N-03 + N-05） | ⏳ 待执行 | column_exists MySQL 占位符 + rand 0.9 |
| M4 HubError 具体化（N-06） | ⏳ 待执行 | UpstreamHttp / UpstreamParse 变体 |
| M5 handle_hub_command 拆解（N-07） | ⏳ 待执行 | 命名函数提取 + 单测覆盖 |

## 环境

- 分支：`refactor/arch-cleanup-p1`
- Worktree：`/Users/kongjie/projects/ilink-hub/.worktrees/refactor/arch-cleanup-p1/`
- Exec-plan：`docs/exec-plans/active/arch-cleanup-p1/`
- E2E capable：false（Rust daemon，无 HTTP E2E 框架）
- flowx 质量门：`cargo fmt --check` / `cargo clippy -- -D warnings` / `cargo test` / `cargo build`

## 恢复指引

```bash
cd /Users/kongjie/projects/ilink-hub
flowx force-dev --feature arch-cleanup-p1 --repo .worktrees/refactor/arch-cleanup-p1
# 或续跑：
flowx force-dev --run-id <上次的 run-id> --repo .worktrees/refactor/arch-cleanup-p1
```

## 关联活跃计划

- `security-p1`：在 `feat/security-p1` 分支，独立推进，无冲突
- `desktop-bridge-profiles`：已完成，待归档
