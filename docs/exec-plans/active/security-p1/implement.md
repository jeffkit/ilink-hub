# security-p1 implementation log

> This file is the chronological work log for the security-p1 plan
> (`docs/exec-plans/active/security-p1/plan.md`). Each milestone
> records what was done, what was found, and any decisions that
> diverged from the plan.

## Status

| Milestone | Description                    | Status   |
| --------- | ------------------------------ | -------- |
| M1        | vtoken 哈希存储                  | ✅ done   |
| M2        | bot_token 静态加密               | ⏳ todo   |
| M3        | quote_index 启动预热            | ⏳ todo   |
| M4        | 迁移兼容                         | ⏳ todo   |
| M5        | 测试                             | ⏳ todo   |
| M6        | 文档与回归                       | ⏳ todo   |

## M1 — vtoken 哈希存储 (2026-06-21)

### 实现摘要

- 新增 `src/hub/vtoken_hash.rs`：单一职责的 SHA-256 hex 助手。
  - `pub fn hash_vtoken(plain: &str) -> String`：SHA-256 → 64 位小写 hex。
  - `pub fn is_vtoken_hash(s: &str) -> bool`：用于 M4 迁移判断行是否已经哈希。
  - 自身携带 6 个单元测试覆盖长度/格式/已知向量/边界。
- `src/hub/registry.rs`：
  - `ClientInfo::vtoken` 字段语义由"明文"改为"SHA-256 hex"。
  - `register()` 返回类型从 `(String, bool)` 扩展为
    `(plaintext, hashed, is_new)`；新注册返回新生成的明文与
    hash，同名重复注册返回空明文与已存 hash。
  - `register_with_vtoken(name, label, Some(hashed))` 是启动加载
    路径：不二次哈希。
  - `get_by_vtoken` / `mark_online` / `mark_offline` 的契约统一：
    vtoken 形参 = hash。`/workspace` 库方（`register` / 路由 / 队列
    / store）只携带 hash。
- `src/server/routes.rs::extract_vtoken`：
  HTTP 边界做一次 `hash_vtoken` 转换；下游所有 `get_by_vtoken`、
  `mark_online`、`poll_tracker.enter`、队列 key 都直接吃 hash。
- `src/server/pairing.rs::register_client_in_hub`：
  返回 `RegisterClientOutcome { plaintext, hashed, is_new }`。
  - `outcome.plaintext` 返回给 bridge（仅此一处）。
  - `state.store.upsert_client(&hashed, ...)` 写入 hash。
  - `rollback_speculative_register(..., &hashed)` 在哈希空间做
    CAS（与 `info.vtoken == vtoken` 比对，二者都是 hash）。
- `src/store/store_tests.rs`：新增 5 个 M1 契约测试。
- 测试集合：
  - `tests/hub_routing_integration.rs` 与 `tests/breaking_changes.rs`
    适配新的 `register()` 返回类型；发送 Authorization header 的
    测试改用 `outcome.plaintext`。
  - `rollback_cas_aborts_when_legit_re_register_happened` 的替换
    客户端改用 64 位 hex 字符串（hash 形态）触发 CAS 不命中。
- `Cargo.toml`：把 `ring` 显式列为直接依赖（原本只通过
  rustls/sqlx 间接引入；M2 还要用 `ring::aead`）。

### 决策分歧（与 plan 的差异）

1. **哈希落点**。plan 第 11/19 行写"`Store` 写入路径在拿到
   明文 vtoken 时先 `hash_vtoken` 再 bind"。我们没有把哈希内
   置到 store 方法里，而是把哈希集中到两个边界：`extract_vtoken`
   （HTTP 入站）和 `register`（注册出站）。原因：
   - store 内部的所有调用方（`commands.rs::set_route`、
     `dispatch.rs::save_message`、`queue.push` 等）拿到的都是
     来自 registry 的 hash；如果让 store 再哈希一次会变成
     双重 hash。
   - 把哈希做成显式的两步，调用方一望即知是 hash 还是
     plaintext，注释和测试都更易读。
   - 契约等价：DB 落盘的全部是 64 位 hex，无明文泄漏。

2. **`register()` 改三返回**。plan 没有显式要求，但
   `register_client_in_hub` 的调用方现在需要同时拿到 plaintext
   （给 bridge）和 hashed（给 store / 队列 / registry）。
   - 三返回 `(plaintext, hashed, is_new)` 在所有调用点都用
     `let RegisterClientOutcome { plaintext, hashed, is_new } = ...`
     的形式解构，编译期就能防止"忘记哈希"或"忘记返回明文"。
   - 配套增加 `RegisterClientOutcome` 结构体作为该三返回的命名
     类型，避免签名 `-> (String, String, bool)` 在 destructure
     时的脆弱性。

3. **`admin_clients` 的 vtoken 显示**。该 endpoint 把
   `c.vtoken.chars().take(8)` 作为可读前缀。M1 之后 `c.vtoken`
   是 hash，前 8 个字符失去可读性（"vhub_xxxx" 不再可见）。
   - 保持 8 字符（32 bit 熵）以保持 admin UI 现有长度；
   - 未来可考虑在 admin UI 中加 `name` 作为主标识，vtoken 仅
     作为可复制的短码。这不在 M1 范围。

### 验证

```
$ cargo fmt --check
(clean)

$ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)

$ cargo test
test result: ok. 371 passed; 0 failed   (lib)
test result: ok.  20 passed; 0 failed
test result: ok.  10 passed; 0 failed
test result: ok.  27 passed; 0 failed
test result: ok.   1 passed; 0 failed
test result: ok.  18 passed; 0 failed

$ cargo build
    Finished `dev` profile

$ cd desktop/ilink-hub-desktop && npm run build
vite v6.4.3 building for production...
✓ 7 modules transformed.
dist/index.html                 19.72 kB
dist/assets/index-dt2WSZD0.css  27.41 kB
dist/assets/index-DxFIEYEF.js   25.69 kB
✓ built in 94ms

$ cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml
    Finished `dev` profile

$ cargo test --lib registry
test result: ok. 9 passed; 0 failed

$ cargo test --lib store_tests
test result: ok. 53 passed; 0 failed
```

### E2E Checkpoint

- **E2E-1（vtoken hash 落盘）**：`m1_upsert_client_stores_hash_not_plaintext`
  通过；`is_vtoken_hash(&row.vtoken)` 为 true；落盘值不是 `vhub_`
  开头。`m1_messages_table_keys_by_hash` 验证了 messages 表的
  vtoken 字段也是 hash。
- 其它 E2E-2/3/4/5 属于 M2/M3/M4/M5。

### 已知遗留 / 后续

- 启动加载路径 `load_clients_from_db` 假设 DB 中 vtoken 已经是
  hash；M1 写出的 DB 自然满足。M4 迁移需要保证旧 DB（vtoken 是
  明文）先被就地哈希，再被启动路径消费。
- 任何上游模块（`bridge/connection.rs`、
  `bridge/dispatcher.rs`、`bridge/manager.rs`）拿到的还是明文
  vtoken；这是正确的——bridge 持有 bearer 凭据是预期行为。Hub
  侧只看到 hash。
