# status.md — mutation-test-coverage-p2

## 当前状态

**进度**：Phase 5–6 完成；2026-07-09 基建修复 + commands/dispatch/store 刷新 ✅；store/clients + store/context 补测 ✅

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1–M3 + Phase 6 | ✅ done | 见历史记录 |
| 基建修复 2026-07-09 | ✅ done | examine/exclude 冲突已修 |
| commands/dispatch 补测 | ✅ done | commands 97.4%；dispatch 77.3% |
| store 层初扫 | ✅ done | sessions 85.3%、messages 95.7% |
| store/clients 补测 | ✅ done | **40% → 100%**（6 个新测试） |
| store/context 补测 | ✅ done | **72.7% → 94.3%**（单测 + 2 集成测试） |

## 分支信息

- 分支：`chore/mutation-testing-infra-fix`（PR #20）
- 工作目录：`/Users/kongjie/projects/ilink-hub`
- Exec-plan：`docs/exec-plans/active/mutation-test-coverage-p2/`

## 刷新摘要

| 模块 | Score |
|------|-------|
| login / crypto / paths / clients | **100%**（或 exclude 后 ~100%） |
| commands | **97.4%**（exclude 后 ~100%） |
| dispatch | **77.3%**（≥75% 目标） |
| store/sessions | **85.3%** |
| store/messages | **95.7%** |
| store/context | **94.3%** |
| dispatch 余下 missed | 10 个 ← 下一优先 |

## 恢复指引

1. 单文件必须 `--no-config`
2. `RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 --file <path> --output mutants-output/<name>`
3. 下一步：补测 dispatch 余下 missed；或 store/sessions/messages 细化
