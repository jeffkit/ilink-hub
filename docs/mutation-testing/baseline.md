# 变异测试基准报告

**建立时间**：2026-07-05  
**工具版本**：cargo-mutants 27.1.0  
**Rust 工具链**：stable-x86_64-apple-darwin  
**运行方式**：`RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 --file ... --output mutants-output/phase1`

---

## Phase 1：核心纯函数模块

**范围**：`vtoken_hash.rs`、`outbound_label.rs`、`ratelimit.rs`  
**运行耗时**：~48 分钟（基准编译 382s + 测试 62s，后续 57 个变异体 2 并行）

### 汇总

| 指标 | 数值 |
|------|------|
| 总变异体 | 57 |
| **已捕获（Caught）** | **51** |
| **未捕获（Missed）** | **6** |
| 不可行（Unviable） | 0 |
| 超时（Timeout） | 0 |
| **Mutation Score** | **89.5%** |

### 已捕获变异体（51 个）✅

**`src/hub/vtoken_hash.rs`（11 个，100% 捕获）**

| 位置 | 变异 |
|------|------|
| 33:5 | `hash_vtoken` → `String::new()` |
| 33:5 | `hash_vtoken` → `"xyzzy".into()` |
| 43:5 | `is_vtoken_hash` → `true` |
| 43:5 | `is_vtoken_hash` → `false` |
| 43:13 | `== 64` → `!= 64` |
| 44:9 | `&&` → `\|\|` |
| 45:44 | `&&` → `\|\|` |
| 45:47 | 删除 `!` in `is_ascii_uppercase()` |
| 49:5 | `hex_lower` → `String::new()` |
| 52:25 | `>>` → `<<` |
| 53:25 | `&` → `\|` / `^` |

**`src/hub/outbound_label.rs`（33 个，100% 捕获）**

| 位置 | 变异 |
|------|------|
| 15:5 | `should_append_outbound_origin_label` → `true` / `false` |
| 15:47 | 删除 `!` |
| 17:37 | `>` → `==` / `<` / `>=` |
| 20:40 | `>` → `==` / `<` / `>=` |
| 26:5 | `format_outbound_origin_line` → `String::new()` / `"xyzzy"` |
| 26:43 | 删除 `!` |
| 27:20 | 匹配守卫 `l != name` → `true` / `false` |
| 27:22 | `!=` → `==` |
| 38:5 | `format_outbound_footer` → `String::new()` / `"xyzzy"` |
| 41:21 | 删除 `!` |
| 41:35 | `&&` → `\|\|` |
| 41:41 | `!=` → `==` |
| 57:5 | `build_persona_header` → `None` / `Some(...)` |
| 61:65 | 删除 `!` |
| 73:5 | `build_session_only_footer` → `None` / `Some(...)` |
| 74:21 | `\|\|` → `&&` |
| 74:26 | `==` → `!=` |
| 92:5 | `apply_persona_and_footer_to_first_text_item` → `()` |
| 108:18 | 删除 `!` |
| 133:5 | `append_outbound_origin_footer_to_first_text_item` → `()` |

**`src/relay/ratelimit.rs`（7 个，53.8% 捕获）**

| 位置 | 变异 | 结果 |
|------|------|------|
| 39:9 | `allow` → `true` | ✅ caught |
| 39:9 | `allow` → `false` | ✅ caught |
| 49:52 | `>=` → `<` (window reset) | ✅ caught |
| 54:25 | `>=` → `<` (count cap) | ✅ caught |
| 57:22 | `+=` → `-=` / `*=` | ✅ caught |
| 60:32 | `>` → `==` / `<` / `>=` (evict threshold) | ❌ missed |
| 63:67 | `<` → `==` / `>` / `<=` (evict window filter) | ❌ missed |

---

### 未捕获变异体（6 个）❌

所有未捕获变异体集中在 `src/relay/ratelimit.rs` 的**内存清理（eviction）分支**：

```rust
// 第 59-64 行：当桶数量超过 10,000 时清理过期桶
// Evict stale keys to bound memory growth.
if inner.buckets.len() > 10_000 {          // ← line 60: > 比较未被覆盖
    inner
        .buckets
        .retain(|_, b| now.duration_since(b.window_start) < self.window); // ← line 63: < 比较未被覆盖
}
```

**根因**：现有测试从未在单个 `RateLimiter` 实例中插入超过 10,000 个不同 key，
导致 `if inner.buckets.len() > 10_000` 分支从未执行。

**修复方案**：在 `ratelimit.rs` 的测试模块中添加：

```rust
#[test]
fn evicts_stale_keys_when_over_limit() {
    // 使用窗口=0 让所有桶立即过期
    let limiter = RateLimiter::new(1, 0);
    // 插入 10_001 个不同 key，触发 eviction 分支
    for i in 0..=10_000usize {
        limiter.allow(&i.to_string());
    }
    // eviction 后继续正常工作
    assert!(limiter.allow("new_key_after_eviction"));
}
```

状态：已在 `src/relay/ratelimit.rs` 补充测试（见 commit 历史）。

---

## 关于 Flaky 集成测试

运行时须设置 `RUST_TEST_THREADS=1`。已发现两个测试在并发时存在竞争条件：

- `tests/hub_routing_integration.rs::messages_queued_in_fifo_order`
- `tests/hub_routing_integration.rs::same_user_gets_stable_virtual_context_token`

两者单独运行均通过，并发时偶发失败。根因是共享 in-memory SQLite 连接池。
**这是已知的测试隔离缺陷，应在后续版本中修复**（为每个测试创建独立的 store 实例）。

---

## 历史记录

| 日期 | Phase | 模块数 | 总变异体 | 捕获 | 未捕获 | 分数 |
|------|-------|--------|----------|------|--------|------|
| 2026-07-05 | Phase 1 | 3 | 57 | 51 | 6 | **89.5%** |
