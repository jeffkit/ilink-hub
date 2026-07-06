# prompt.md — mutation-test-coverage-p2

## 目标

将变异测试覆盖范围扩展至 Hub 核心业务逻辑模块，针对 Phase 1-3 未覆盖的高价值文件
建立测试基线，持续提升 Mutation Score。

## 背景

Phase 1-3 完成后，`mutants.toml` 已覆盖 7 个纯函数/状态管理模块，Mutation Score 均超过 80%。
以下三个高优先级模块尚未纳入：

1. `src/hub/dispatch.rs`（1268 行）— 消息分发核心，分支最密集
2. `src/hub/commands.rs`（467 行）— 命令解析逻辑
3. `src/hub/pairing.rs`（465 行）— 配对流程逻辑

另外 `src/relay/auth.rs` 已在 Phase 4（本 plan 之前）单独处理。

## 约束

- 纯测试添加，不修改生产代码
- 必须通过 `cargo fmt --all -- --check` + `cargo clippy -- -D warnings`
- 变异测试运行命令：`RUST_TEST_THREADS=1 cargo mutants -j 2 --file <path> --output mutants-output/<phase>`
- 每个里程碑独立验证后才推进下一个
