# P1 可靠性修复 (DB-01, E-03) Implement Log

## M1 — DB-01: SQLite AnyPool 多连接并发问题修复

### Decisions

- 把 `Store::connect` 里 pool 分支的判断从 `url.contains(":memory:")`
  改为 `url.starts_with("sqlite:")`，让所有 `sqlite:` URL（file 和
  `:memory:`）统一走 `max_connections(1)`。
- 保留 sqlx `SqliteConnectOptions::new` 默认的 5s `busy_timeout` 不变，
  作为「shutdown 时 migration runner 仍在 acquire 排空」这种边缘场景
  的安全网。单连接 + 默认 busy_timeout 已经能完全避免 file-level
  EXCLUSIVE write lock 的多连接竞争，busy_timeout 退居二线。
- 把测试断言改为「结构性 invariant」：`store.pool.options().get_max_connections() == 1`。
  这是可重复、不依赖磁盘速度的回归护栏；之前单纯跑并发事务无法
  稳定触发 5s 锁等待，CI 里看不到 SQLITE_BUSY 也不能算覆盖到。
- 新增的并发负载测试同时覆盖 `persist_context_tokens_batch` /
  `set_active_session_name` / `get_active_session_name` 三类操作，
  复现 plan §M1 的「手动复现」步骤。

### Problems

- 第一次写测试时只跑 read+write，5s 默认 `busy_timeout` 把 busy 错误
  吞掉了，CI 里通过等于不通过。改为结构断言后，revert 修复时测试
  立刻以 `left: 10, right: 1` 失败，验证了护栏确实有效。
- 写测试时一开始用了 `format!("sqlite://{}", path)`，但 sqlx 的
  URL 解析会把单条 `//` 后面的内容当成 host，导致 `unable to open
  database file`（code 14）。改用项目里既有的 `format!("sqlite:{}", path)`
  形式（与 `tests/breaking_changes.rs:186` 一致）后通过。
- clippy 在测试里点出了字符串字面量误用 `{r}` / `{i}` 占位符的
  lint（普通字符串不是 format!），改为 `format!` 后清掉。

### Outcome

- `src/store/mod.rs::Store::connect` 的 pool 分支改为对所有 `sqlite:`
  URL pin `max_connections(1)`；doc comment 写清楚 file-level write
  lock 的原因和 5s busy_timeout 的角色。
- 新增 `store::store_tests::file_sqlite_serializes_concurrent_read_and_write_without_busy`：
  - 先断言 `pool.options().get_max_connections() == 1`（结构护栏，
    revert 修复会立即以 `left: 10, right: 1` 失败）；
  - 然后跑 8 个 batch-writer × 20 轮 × 200 行 + 4 个单行 writer ×
    200 轮 + 4 个 reader × 200 轮的并发负载（multi-thread runtime,
    8 workers），全部 join 不应返回 `SQLITE_BUSY`。
- 四条质量门禁全绿（fmt / clippy / cargo test / cargo build）：
  - cargo test：lib 122 + breaking_changes 7 + hub_routing_integration 9
    + queue_trait_tests 10 = 148 通过，0 失败。
- 写完 `docs/exec-plans/active/todo-reliability-p1/reviews/m1/review-request.yaml`，
  与上一份 todo-security-p1 的 m1 review 模板保持同样的字段结构。
