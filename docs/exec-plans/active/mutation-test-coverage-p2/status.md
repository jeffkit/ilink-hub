# status.md — mutation-test-coverage-p2

## 当前状态

**进度**：Phase 5–6 完成；2026-07-09 基建修复 ✅

| 里程碑 | 状态 | 说明 |
|--------|------|------|
| M1–M3 + Phase 6 | ✅ done | 见历史记录 |
| 基建修复 2026-07-09 | ✅ done | examine/exclude 冲突已修；login/crypto 刷新达标 |

## 分支信息

- 分支：`chore/mutation-testing-infra-fix`
- 工作目录：`/Users/kongjie/projects/ilink-hub`
- Exec-plan：`docs/exec-plans/active/mutation-test-coverage-p2/`

## 2026-07-09 基建修复摘要

- `cargo mutants --list-files` = **45**（修复前有效仅 ~27）
- login：**0% → 90.9%**（exclude_re 后 ~100%）；crypto：**100%**
- 下一优先：`bridge/paths.rs`（33.3%，8 missed）

## 恢复指引

1. 单文件必须 `--no-config`（v27+ 会把 `--file` 与 examine_globs 合并）
2. `RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 --file <path> --output mutants-output/<name>`
3. 全量：推送后依赖 `.github/workflows/mutation-testing.yml` 周跑
4. 下一步：补测 `bridge/paths.rs`；再处理 mock upstream 暂缓项
