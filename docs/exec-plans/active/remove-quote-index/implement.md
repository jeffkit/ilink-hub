# implement.md: remove-quote-index

## M1 — 删除内存路由层

### Decisions
- 将 `QuoteRouteIndex::collect_quoted`、`collect_quoted_timestamp`、`footer_from_user_quote` 从实例方法转为模块级独立公共函数，保留函数签名不变，dispatch.rs 中调用路径从 `quote_route::QuoteRouteIndex::xxx` 更新为 `quote_route::xxx`。
- 删除 `HubState::quote_index_warmed` 字段，同步删除 `/metrics` 中 `ilink_hub_quote_index_ready` gauge；该指标与内存索引强绑定，移除更干净。
- dispatch.rs 的 `quoted` 计算块原先有 scope 构建逻辑，删除内存查找后 scope 变量不再被使用，一并清除。
- 保留了 `QuoteOrigin::Hub` 变体和相关 merge 逻辑，因为 commands.rs 中仍需要区分 Hub 命令来源（虽然不再 register，但 QuoteOrigin::Hub 作为 fallback 不受影响）。
- 删除 `sendmessage_quote_index_uses_hub_ext_session_not_db_active` 及 `sendmessage_without_hub_ext_session_falls_back_to_db_active_session` 两个测试，因为它们直接操作 `state.routing.quote_index`，与被删除的 incident 复现逻辑绑定。

### Problems & Solutions
- routes.rs 的 `register_outbound_content` 调用块删除后，`conv_scope` 变量成为悬空 unused let，clippy 会报 warning → 同步删除 `conv_scope` 赋值行。
- `cargo fmt` 检测到 `collect_quoted` 函数签名换行格式不符合 rustfmt 规范，以及 serve.rs 的 import 分组 → 执行 `cargo fmt --all` 自动修复。

### Outcome
- 验证通过：`cargo build` ✅ / `cargo test` ✅ / `cargo clippy -- -D warnings` ✅ / `cargo fmt --all -- --check` ✅
- 净削减约 1714 行代码（+69 / -1714）
- Commit: 4fbeb89

---

## M2 — 新增 DB fallback 回归测试

### Decisions
- 新增 `at_mention_quote_reply_l3_footer_routing` 测试，与已有的 L1/L2 测试并列于 `dispatch.rs` 末尾的 `tests` 模块中，统一测试风格。
- 选择在同一 in-memory state 上额外注册 "ilink-claude" 客户端（调用 `make_state_with_client()` 后追加注册），避免重复实现 state 构建逻辑。
- 使用 `resolve_quote_from_footer` 作为直接入口（而非 `resolve_quote_from_db`），因为 M2 目标是覆盖 L3 footer 路径的快速注册表查找逻辑。
- Footer 文本格式 `Some reply text\n\n---\nilink-claude · at-20260704-103000` 严格遵循 `parse_footer_from_quoted_text` 的 `\n---\n` 分隔符约定。

### Problems & Solutions
- `cargo fmt` 检测到 `let (_, ilink_claude_vtoken, _) = state.clients.registry...` 的链式调用需要缩进换行 → 执行 `cargo fmt --all` 自动修复。

### Outcome
- 验证通过：`cargo test at_mention_quote_reply_l3` ✅ / `cargo test -- --test-threads=4` ✅ / `cargo clippy -- -D warnings` ✅ / `cargo fmt --all -- --check` ✅
- 新增 56 行（+56 / -0），覆盖 L3 footer fallback 的快速路径（registry lookup）
- Commit: 2d69677
