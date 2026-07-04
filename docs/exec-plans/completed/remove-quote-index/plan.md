# Plan: remove-quote-index

## 架构设计

```
当前架构（4层）:
  inbound quote-reply
       ↓
  [L1] QuoteRouteIndex (内存)    ← 删除
       ↓ miss
  [L2] DB 时间戳查询              ← 保留
       ↓ miss
  [L3] DB 内容前缀查询            ← 保留
       ↓ miss
  [L4] footer 文本解析            ← 保留

目标架构（3层）:
  inbound quote-reply
       ↓
  [L1] DB 时间戳查询
       ↓ miss
  [L2] DB 内容前缀查询
       ↓ miss
  [L3] footer 文本解析
```

变更范围：
- `src/hub/quote_route.rs`：删除 QuoteRouteIndex + ContentEntry + WarmItem，保留 QuoteOrigin/merge_routing_with_quote/parse_footer_from_quoted_text/collect_quoted_content 等工具函数
- `src/hub/state.rs`：删除 `quote_index: Arc<Mutex<QuoteRouteIndex>>` 字段及 RoutingState 中的引用
- `src/hub/dispatch.rs`：删除 `spawn_quote_index_evictor`，删除 L1 in-memory lookup，只保留 L2/L3/L4 DB fallback
- `src/server/routes.rs`：删除 `register_outbound_content` 调用
- `src/runtime/serve.rs`：删除 warm_from_history 预热逻辑
- `src/store/messages.rs`：保留（DB fallback 依赖），可删除 `recent_outbound_messages` 函数（仅 warmup 使用）

## 里程碑

### M1 — 删除内存路由层

**目标**：移除 QuoteRouteIndex 及其所有引用，代码编译通过，现有测试通过

**变更文件**：
- `src/hub/quote_route.rs`
- `src/hub/state.rs`
- `src/hub/dispatch.rs`
- `src/server/routes.rs`
- `src/runtime/serve.rs`
- `src/store/messages.rs`（可能）

**验证命令**：
```bash
cargo build 2>&1 | grep -E "^error"
cargo test -- --test-threads=4 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | grep -E "^error"
```

**E2E checkpoint：** not-ready
**E2E 判定依据：** e2e-protocol Step B — 纯内部模块重构，无新增外部 HTTP endpoint，不满足 "新增可测试外部接口" 条件
**E2E 场景：** N/A
**Visual Review：** not-needed

---

### M2 — 新增 DB fallback 回归测试

**目标**：在 `src/store/store_tests.rs` 或 `src/hub/tests.rs` 中新增集成测试，覆盖三层 DB fallback 的端到端路由逻辑

**测试场景**：
1. `test_quote_route_via_timestamp`：消息保存 DB → 通过 create_time_ms ±10s 窗口找到正确 vtoken + session_name
2. `test_quote_route_via_content_prefix`：消息保存 DB → 通过内容前缀 48 字符匹配找到正确 vtoken + session_name
3. `test_quote_route_via_footer`：引用消息带有 `---\nbackend-name · session` footer → 通过 footer 解析找到正确 backend

**验证命令**：
```bash
cargo test test_quote_route -- --nocapture 2>&1
cargo test -- --test-threads=4 2>&1 | tail -20
cargo clippy -- -D warnings 2>&1 | grep -E "^error"
cargo fmt --all -- --check 2>&1
```

**E2E checkpoint：** not-ready
**E2E 判定依据：** e2e-protocol Step B — 纯 Rust 库项目，无浏览器端点，不适用浏览器 E2E
**E2E 场景：** N/A（`cargo test` 即为集成验证手段）
**Visual Review：** not-needed
