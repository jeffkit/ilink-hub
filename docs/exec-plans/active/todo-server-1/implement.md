# Security Issues Fixes (SEC-006) Implement Log

## Milestone 1: 修复 SEC-006 (Admin token 非常量时间比较)

### Decisions

- 引入 `subtle` v2.6.1 依赖，对 `check_admin_auth` 中的 admin token 进行常量时间比较（Constant-time comparison），防御潜在的计时攻击。
- 在 `src/server/routes.rs` 内部添加 `admin_auth_tests` 单元测试，分别测试错误 Token 以及空 Header 场景下的鉴权行为，同时确保不因为 OnceLock 重复初始化而影响现有集成测试。

### Problems

- 由于 `admin_token()` 使用 `OnceLock` 存储且读取环境变量，在同一个测试进程中多次初始化不同的 token 会产生冲突。因此，我们的单元测试设计为不假设 environment variable 的具体状态，而是针对 wrong token 和 empty header（利用 current insecure mode check）进行正交测试，既覆盖了 auth 校验代码又保证了测试稳定性。

### Outcome

- 修改了 `src/server/routes.rs` 的 `check_admin_auth` 函数，使用 `subtle::ConstantTimeEq` 比较 `provided.as_bytes()` 与 `required.as_bytes()`。
- 在 `Cargo.toml` 中新增了 `subtle` 依赖项。
- 质量门禁全绿：
  - `cargo fmt --check` 通过。
  - `cargo clippy -- -D warnings` 通过，0 warnings/errors。
  - `cargo test` 通过，包含新增的 `admin_auth_tests` 模块（2个新测试，总计 123 个 unit_lib 测试，全部集成测试通过）。
  - `cargo build` 通过。
- 编写并保存了 `docs/exec-plans/active/todo-server-1/reviews/m1/review-request.yaml`。
