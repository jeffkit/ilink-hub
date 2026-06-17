# 数据库迁移版本追踪执行计划

本计划旨在解决 `ilink-hub` 数据库迁移的双轨维护问题：当前 `src/store/mod.rs` 里有手写内联 DDL，而 `migrations/` 目录下也有 SQL 文件，两者不同步，且没有版本表做迁移追踪。

## 里程碑列表与验证命令

### 里程碑 1：建立迁移版本追踪表及测试框架
- **任务目标**：
  1. 在 [src/store/mod.rs](file:///Users/kongjie/projects/ilink-hub/src/store/mod.rs) 中设计 `schema_version` 表结构。
  2. 实现版本控制元数据逻辑，判断特定版本的迁移是否已运行。
  3. 创建数据库测试辅助方法，能获取当前数据库的版本号。
- **验证命令**：
  - 运行单元测试（此时仅验证 `schema_version` 表初始化与读取）：
    ```bash
    cargo test --lib store::tests
    ```
- **E2E Checkpoint 1**：能够在 `sqlite::memory:` 中初始化 `schema_version` 表，并且通过查询确认初始版本号记录正确。

### 里程碑 2：重构 Store::run_migrations 并支持版本管理逻辑
- **任务目标**：
  1. 将已有的 v1 到 v5 迁移拆分为独立的迁移函数/步骤（如 `migrate_to_v1`、`migrate_to_v2` ... `migrate_to_v5`）。
  2. 每个步骤执行完后，必须更新 `schema_version` 表。
  3. 修复 `ALTER TABLE` 或 `CREATE INDEX` 的错误吞掉逻辑：移除 `if let Err(e) = ...` 的 swallow 警告逻辑，若执行失败直接返回 `Result::Err`，确保迁移失败能正确阻断程序启动。
  4. 统一 DDL 中的所有时间戳默认值为 `CURRENT_TIMESTAMP`（替换 `datetime('now')`）。
- **验证命令**：
  - 编译项目：
    ```bash
    cargo build
    ```
  - 运行完整测试，特别包含并发与持久化测试：
    ```bash
    cargo test --lib store::tests
    ```
- **E2E Checkpoint 2**：正常连接数据库时，所有的迁移步骤（v1-v5）都能幂等成功执行。若中途某一步发生失败，`Store::connect` 将返回错误，阻断后续的 DDL 执行和程序启动。

### 里程碑 3：同步与对齐 `migrations/` 下的 SQL 文件
- **任务目标**：
  1. 修改 `migrations/0001_initial_schema.sql`、`0002_backend_sessions.sql`、`0004_context_token_map_created_at.sql`，将所有的 `datetime('now')` 替换为 `CURRENT_TIMESTAMP`，保持与代码一致。
  2. 新增 `migrations/0005_messages.sql`，包含 `messages` 表的创建及索引创建语句，以与代码中的 v5 迁移对齐。
  3. 新增 `migrations/0000_schema_version.sql`，用于文档化/同步 `schema_version` 表的创建。
- **验证命令**：
  - 使用 `diff` 检查 SQL 语句与 Rust 代码中的 SQL 字符串内容是否一致。
  - 确保整个项目的静态检查和格式化通过：
    ```bash
    cargo clippy -- -D warnings
    ```
- **E2E Checkpoint 3**：验证 `migrations/` 下的所有 SQL 文件定义 and Rust 源代码中对应的 DDL 语句完全对齐（特别是字段类型、默认值时间戳 `CURRENT_TIMESTAMP` 以及索引命名）。

### 里程碑 4：添加专门的迁移幂等性与错误处理测试用例
- **任务目标**：
  1. 在 `src/store/mod.rs` 的测试模块中，编写测试用例 `test_migration_idempotency`，多次调用 `run_migrations`，确认不会报错且版本号维持在最新版。
  2. 编写测试用例 `test_migration_failure_propagation`，模拟迁移中途出错，验证错误能被正确向上抛出。
  3. 编写测试用例 `test_migration_incremental`，模拟数据库处于旧版本（如 v2），验证执行 `run_migrations` 时能正确执行 v3, v4, v5 迁移，并最终将 `schema_version` 更新为最新版本。
- **验证命令**：
  - 运行所有 store 测试：
    ```bash
    cargo test --lib store::tests
    ```
  - 运行 Clippy 零警告检查：
    ```bash
    cargo clippy --all-targets -- -D warnings
    ```
- **E2E Checkpoint 4**：所有新增的迁移测试用例通过，验证版本迁移系统在各类边界条件下的鲁棒性。

## E2E Checkpoints 汇总

- **Checkpoint 1** (里程碑 1): 能够在内存数据库中成功初始化 `schema_version` 并读取初始版本。
- **Checkpoint 2** (里程碑 2): 完成 v1-v5 迁移的函数化拆分，任何 DDL 语句执行失败都会导致 `Store::connect` 返回 `Err`，不发生静默失败。
- **Checkpoint 3** (里程碑 3): `migrations/` 目录下的 SQL 文件与 Rust 内联 DDL 实现了完全的同步与对齐，并统一使用 `CURRENT_TIMESTAMP`。
- **Checkpoint 4** (里程碑 4): 迁移幂等性、增量升级和错误传播的单元/集成测试用例全部绿灯通过。
