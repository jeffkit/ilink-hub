修复 store 模块修复：SYNC-02, DB-03, DB-02

## 待修复条目

  - [SYNC-02] bridge 重注册同名新 vtoken 后 routing_state 残留旧 vtoken
     文件：src/store/mod.rs:195-213, upsert_client
     问题：`upsert_client` 在 `ON CONFLICT (name) DO UPDATE SET vtoken = EXCLUDED.vtoken` 时更新了 clients 表的 vtoken，但 `routing_state` 表中 `active_vtoken` 字段指向旧 vtoken 的行不会被同步更新。bridge 重启后用新 vtoken 注册，原来通过 `/use` 选择该 
     修复方向：`upsert_client` 执行后，在同一事务中执行 `UPDATE routing_state SET active_vtoken = $new_vtoken WHERE active_vtoken = $old_vtoken`。

  - [DB-03] get_hub_ext_batch 行值 IN 语法不兼容 MySQL
     文件：src/store/mod.rs:432-479
     问题：`WHERE (vctx, vtoken) IN (($1,$2), ($3,$4), ...)` 的行值构造语法在 SQLite 3.15+ 和 PostgreSQL 支持，但 MySQL 5.x 不支持（MySQL 8.0 支持）。若用户使用 MySQL 5.7，该查询直接报错导致 Broadcast 路径的 HubExt 全部降级为 None。
     修复方向：改为等价的 `OR` 子句：`WHERE (vctx = $1 AND vtoken = $2) OR (vctx = $3 AND vtoken = $4) OR ...`；或检测数据库类型选择不同 SQL。

  - [DB-02] persist_context_tokens_batch 事务持有时间随 N 增大
     文件：src/store/mod.rs:598-624
     问题：在 Broadcast 场景下，事务持有时间 = N × 单条 upsert 耗时，N 较大时 SQLite write lock 被长期占用，阻塞其他写操作。
     修复方向：将批量写入分块（每批 50 条），或改用 `INSERT ... VALUES (...),(...),(...) ON CONFLICT DO UPDATE` 多值批量语法，减少事务往返次数。

## 完成标准
- [ ] SYNC-02 修复已提交，相关测试通过
- [ ] DB-03 修复已提交，相关测试通过
- [ ] DB-02 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖