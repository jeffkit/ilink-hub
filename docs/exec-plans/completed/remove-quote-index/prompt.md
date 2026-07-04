# remove-quote-index

## 目标

移除 ilink-hub 中的内存 QuoteRouteIndex，让引用消息（quote-reply）的路由完全依赖三层 DB fallback（时间戳查询 → 内容前缀查询 → footer 文本解析），消除因内存状态冷启动、流式消息污染 index、10000 key 上限等导致的路由出错问题。

## 完成标准

1. `src/hub/quote_route.rs` 中的 `QuoteRouteIndex` 结构体及所有内存相关代码（`register_outbound_content`、`warm_from_history`、`spawn_quote_index_evictor`、TTL/eviction 逻辑）已删除
2. `src/server/routes.rs` 中的 `register_outbound_content` 调用已删除
3. `src/hub/dispatch.rs` 中的内存 index 查询路径（`resolve_user_quote`）已删除，只保留 3 层 DB fallback
4. `src/hub/state.rs` 中的 `quote_index` 字段已删除
5. `src/runtime/serve.rs` 中的 warmup 预热逻辑已删除
6. `cargo test` 全部通过（包括现有路由测试）
7. 新增至少 3 个集成测试，分别覆盖：时间戳 fallback、内容前缀 fallback、footer fallback 三条路径

## 非目标

- 不修复 `should_append_outbound_origin_label` 在单 backend 场景下不加 footer 的问题（单独 issue）
- 不迁移到 MySQL（保持 SQLite 支持）
- 不修改三层 DB fallback 的业务逻辑，仅删除内存层

## 硬约束

- SQLite 部署，`strftime('%s', created_at)` 可用
- 删除后 `cargo clippy -- -D warnings` 必须零警告
- 删除后 `cargo fmt --all -- --check` 必须通过
