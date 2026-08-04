# ilink-hub 卫生铁律

> 本文件被 pge flow 的 Generator/repair prompt 自动读取并注入（见 pge.flow.js 的 `loadHygiene`）。
> 改动这里 = 改变 Generator 在本仓写代码时遵守的规矩。

## Rust 工程铁律（违反即视为失败）

- **新模块必须注册**：创建 `src/<mod>/` 必须同时加 `src/<mod>/mod.rs` 且在 `src/lib.rs`
  （或 `src/main.rs`）里 `pub mod <mod>;`——否则 cargo 根本不编译，等于死代码、测试也跑不到。
- **禁止用 `cargo run` / `cargo build` 创建临时探查二进制**（如 `*_check` / `*_explore`）。
  要探查类型用 `cargo expand` 或写到 `#[cfg(test)] mod tests` 里。
  worktree git status 必须干净，不允许遗留可执行文件。
- **涉及外部 crate 时必须先读源码确认 API 形状**：
  - crate 的真实定义在 `~/.cargo/registry/src/*/crate-name-*/src/` 下
  - 不许凭印象写 struct 字段、enum 变体名、untagged enum discriminator
  - 写 serde 测试时，JSON 字面量字段名/必填字段必须按真实 schema 来
- **Cargo.toml 新依赖**：参考已有 optional 依赖的写法。`cargo update` 后必须将
  `Cargo.lock` 一并提交（Docker 构建使用 `--locked`，Cargo.lock 缺失或不一致会失败）。
- **编译验证**：实现完成后先跑 `cargo build`（或 `PATH="$HOME/.cargo/bin:$PATH" cargo build`）
  确认编译过，再交回——在质量门之前发现编译错，省一次 evaluator 调用。

## 本仓特定规矩（来自 AGENTS.md）

- **生产路径禁止裸 `unwrap()`**：用 `thiserror` + `?` 传播错误。测试代码里可用。
- **clippy 零 warning 容忍**：`-D warnings`，提交前必须全部清除。
- **DB 集成测试必须串行**：`cargo test -- --test-threads=1`，用 `DATABASE_URL=sqlite::memory:`
  内存数据库，避免并发状态污染。
- **特性开发禁止在 main 直接提交**：通过 force-dev worktree 隔离。
- **commit 禁止添加 `Co-authored-by`** 信息。
- **feature flags**：默认 `sqlite`；`postgres`/`mysql` 为可选。改动涉及 DB 后端时
  用 `--all-features` 验证全量编译。
