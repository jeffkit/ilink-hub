# 目标

修复 ilink-hub 数据库迁移的双轨维护问题：当前 `src/store/mod.rs` 里有手写内联 DDL，同时 `migrations/` 目录下也有 SQL 文件，两者不同步，且没有版本表做迁移追踪。

## 问题

- `src/store/mod.rs:93-198`：`Store::run_migrations` 手写 `CREATE TABLE IF NOT EXISTS`，ALTER TABLE 失败只是 warn/debug，没有版本表
- `migrations/` 目录下有 `.sql` 文件但未被代码使用（`sqlx::migrate!` 宏未启用）
- SQLite 用 `datetime('now')`，Rust 内联 DDL 用 `CURRENT_TIMESTAMP`，细节不一致
- `AnyPool` 与 `sqlx::migrate!` 宏有兼容性限制

## 修复方向

1. **添加 schema_version 表**：在 `run_migrations` 最开始创建 `schema_version` 表，追踪已运行的迁移编号
2. **将现有 DDL 整理为带版本号的迁移函数**：每个迁移步骤检查 schema_version 决定是否跳过
3. **同步 `migrations/` SQL 文件**：确保 `migrations/*.sql` 与代码中实际运行的 DDL 一致（或在注释中说明它们仅做文档用途）
4. **修复 ALTER TABLE 的静默失败**：失败时返回错误而不仅仅是 debug/warn
5. **统一时间戳**：将所有地方统一为 `CURRENT_TIMESTAMP`

注意：不强求迁移到 `sqlx::migrate!` 宏（AnyPool 兼容性复杂），重点是：版本追踪 + 双轨同步 + 不静默失败。

## 完成标准

- [ ] 有 `schema_version` 表，迁移有幂等性保证
- [ ] ALTER TABLE 失败时返回错误（不再静默忽略）
- [ ] `migrations/*.sql` 文件与代码实现对齐（或有清晰注释说明关系）
- [ ] 时间戳统一为 `CURRENT_TIMESTAMP`
- [ ] `cargo test` 全部通过（含迁移相关测试）
- [ ] `cargo build` 成功
- [ ] `cargo clippy -- -D warnings` 零警告

## 非目标

- 不改变数据库 schema 本身
- 不强求切换到 sqlx::migrate! 宏
- 不修改业务查询逻辑
