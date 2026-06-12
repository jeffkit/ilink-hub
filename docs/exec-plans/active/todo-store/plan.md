# Todo Store Plan

## 范围

涉及存储层 `src/store/mod.rs` 以及与 Hub 调度器交互的稳定性修复。

## 设计

1. **[SYNC-02] 解决 bridge 重注册旧 vtoken 残留问题**
   - 在 `upsert_client` 中开启数据库事务。
   - 插入/更新前先查询已有的同名 `vtoken`（若存在）。
   - 执行 upsert 插入或更新 `clients` 表。
   - 若存在旧的 `vtoken` 且与新 `vtoken` 不同，在同一事务中更新 `routing_state` 表中的 `active_vtoken` 为新值：
     ```sql
     UPDATE routing_state SET active_vtoken = $new_vtoken WHERE active_vtoken = $old_vtoken
     ```
   - 提交事务。

2. **[DB-03] 解决 `get_hub_ext_batch` IN 语法对 MySQL 5.x 不兼容问题**
   - 避免使用 `(vctx, vtoken) IN (($1,$2), ($3,$4), ...)` 语法。
   - 使用等价的 `OR` 展开语法：
     ```sql
     WHERE (vctx = $1 AND vtoken = $2) OR (vctx = $3 AND vtoken = $4) OR ...
     ```
   - 同样在查询 `backend_sessions_v2` 时，使用类似 OR 结构：
     ```sql
     WHERE (vctx = $1 AND vtoken = $2 AND session_name = $3) OR ...
     ```

3. **[DB-02] 解决 `persist_context_tokens_batch` 事务持有时间过长问题**
   - 在 Broadcast 调度场景下，为防止大批量的批量写入长期独占 SQLite 写锁，将输入 entries 分块写入。
   - 设定分块大小为 50 条，每个分块在一个独立的事务中执行写入并提交，从而允许其他写操作在分块提交之间插队执行。

## 验证命令

- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test`
- `cargo build`

## 风险

- 事务的性能开销：每次 chunk 写入会有独立的事务 commit，在 SQLite 上可能稍有 IO 开销，但由于 broadcast 发生频率相对低，对整体性能影响不大。
