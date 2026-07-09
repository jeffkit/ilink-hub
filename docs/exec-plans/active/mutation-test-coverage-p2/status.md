# status.md — mutation-test-coverage-p2

## 当前状态

**进度**：Phase 5–6 完成；2026-07-09 基建修复 + commands/dispatch/store 刷新 ✅

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1–M3 + Phase 6 | ✅ done | 见历史记录 |
| 基建修复 2026-07-09 | ✅ done | examine/exclude 冲突已修 |
| commands/dispatch 补测 | ✅ done | commands 97.4%；dispatch 77.3% |
| store 层初扫 | ✅ done | sessions 85.3%、messages 95.7%；clients 40% 待补 |

## 分支信息

- 分支：`chore/mutation-testing-infra-fix`（PR #20）
- 工作目录：`/Users/kongjie/projects/ilink-hub`
- Exec-plan：`docs/exec-plans/active/mutation-test-coverage-p2/`

## 2026-07-09 刷新摘要

| 模块 | Score |
|------|-------|
| login / crypto / paths / messages | ≥90% |
| commands | **97.4%**（exclude 后 ~100%） |
| dispatch | **77.3%**（≥75% 目标） |
| store/sessions | **85.3%** |
| store/context | 72.7% |
| store/clients | **40%** ← 下一优先 |

## 恢复指引

1. 单文件必须 `--no-config`
2. `RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 --file <path> --output mutants-output/<name>`
3. 下一步：补测 `store/clients.rs`；再处理 dispatch 余下 missed / store/context
