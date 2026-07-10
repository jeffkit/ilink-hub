# status.md — mutation-test-coverage-p2

## 当前状态

**进度**：Phase 5–6 完成；基建修复已合入 main（PR #20）；dispatch 补测 ✅

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1–M3 + Phase 6 | ✅ done | 见历史记录 |
| 基建修复 2026-07-09 | ✅ done | PR #20 已合并 |
| commands/dispatch 补测 | ✅ done | commands 97.4%；dispatch **93.6%**（exclude 后 ~100%） |
| store 层初扫 | ✅ done | sessions 85.3%、messages 95.7% |
| store/clients 补测 | ✅ done | **40% → 100%** |
| store/context 补测 | ✅ done | **72.7% → 94.3%** |

## 分支信息

- 分支：`test/dispatch-mutation-catch-up`
- 工作目录：`/Users/kongjie/projects/ilink-hub`
- Exec-plan：`docs/exec-plans/active/mutation-test-coverage-p2/`

## 刷新摘要

| 模块 | Score |
|------|-------|
| login / crypto / paths / clients | **100%**（或 exclude 后 ~100%） |
| commands | **97.4%**（exclude 后 ~100%） |
| dispatch | **93.6%**（exclude 后 ~100%） |
| store/sessions | **85.3%** |
| store/messages | **95.7%** |
| store/context | **94.3%** |

## 恢复指引

1. 单文件必须 `--no-config`
2. `RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 --file <path> --output mutants-output/<name>`
3. 下一步：全量周跑基线；或 store/sessions 细化；集成测试隔离
