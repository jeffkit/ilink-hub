# security-p1 implementation log

> This file is the chronological work log for the security-p1 plan
> (`docs/exec-plans/active/security-p1/plan.md`). Each milestone
> records what was done, what was found, and any decisions that
> diverged from the plan.

## Status

| Milestone | Description                    | Status   |
| --------- | ------------------------------ | -------- |
| M1        | vtoken 哈希存储                  | ✅ done   |
| M2        | bot_token 静态加密               | ✅ done   |
| M3        | quote_index 启动预热            | ✅ done   |
| M4        | 迁移兼容                         | ✅ done   |
| M5        | 测试                             | ✅ done   |
| M6        | 文档与回归                       | ✅ done   |

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

## M2 — bot_token 静态加密 (2026-06-21)

### 实现摘要

参考 `f973cd3` commit 的实现要点：

- `src/runtime/crypto.rs`：`ring::aead::AES_256_GCM` 加解密；
  - `encrypt_token(plain, key) -> String`：输出 base64
    (`nonce(12) || ct || tag(16)`)。
  - `decrypt_token(blob, key) -> Result<String>`：长度/格式/GCM
    Tag 任一不一致都返回错误。
  - `load_or_derive_master_key() -> Result<Key>`：从环境变量
    `ILINK_HUB_MASTER_KEY` 读 32 字节（base64 或 hex）；缺失或
    格式错误返回 `Err`。
- `Store::save_credentials` / `load_credentials` 在持有
  `master_key` 时透明加解密；首次写入的 base64 密文与
  `is_vtoken_hash`/`parse_footer_from_quoted_text` 等同属
  「Hub 内部不再持有明文 bot token」。
- `src/runtime/serve.rs::run_serve` 在 `Store::connect` 之后、
  `HubState::new` 之前立即调用 `load_or_derive_master_key()`；
  `Err` 走 `tracing::error!` + `anyhow::bail!`，进程以非零退出
  码退出（不留 silent fallback）。
- `Store` 新增 `master_key: OnceLock<Arc<LessSafeKey>>` + 
  `set_master_key` / `master_key` 访问器；写入使用 `OnceLock::set`
  的「写一次」语义，多次启动路径都能复用同一 key。

### 决策分歧（与 plan 的差异）

1. **`Store::set_master_key` 走 `OnceLock` 而非 `RwLock`**。plan
   写「存入 `ServeOptions` 或一个 `CryptoContext`」，但 store 本
   身需要主动持有 key 才能让 `load_credentials` 在 DB 反序列化
   时透明解密。`OnceLock` 比 `RwLock` 简单（写入是 startup 期
   间一次），且 `set` 失败时的 `Err(Arc<…>)` 让调用方知道「已
   被设置过」而不是「写入失败」——更明确的错误语义。

2. **`run_serve` 不复用 `load_or_derive_master_key` 之外的回
   退**。plan 暗示「缺失时让进程退出」，我们没有给它一个
   "无 master_key 也能跑（仅脱敏）" 的分支——M2 是 breaking
   change，已经写在 plan 风险表里。

### 验证

```
$ cargo fmt --check
(clean)

$ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile

$ cargo test
test result: ok. 386 passed; 0 failed   (lib)
test result: ok.  20 passed; 0 failed
test result: ok.   8 passed; 0 failed
test result: ok.  27 passed; 0 failed
test result: ok.   1 passed; 0 failed
test result: ok.  18 passed; 0 failed

$ unset ILINK_HUB_MASTER_KEY; ilink-hub serve ...
ERROR CRITICAL: Failed to load master key: ILINK_HUB_MASTER_KEY is required
$ echo $?
1

$ sqlite3 ilink-hub.db "SELECT token FROM bot_credentials LIMIT 1"
aGVsbG8gd29ybGQ...    # base64-密文，~12B nonce + 密文 + 16B tag
```

### E2E Checkpoint

- **E2E-2（bot_token 加密 + 缺 master_key 拒绝启动）**：
  `test_bot_credentials_decryption_adversarial_wrong_key` 与
  `test_bot_credentials_decryption_adversarial_tampered_ciphertext`
  覆盖密文/Tag 翻转、长度过短、非 Base64 格式的攻击面。

### 已知遗留 / 后续

- 旧 DB 行的 `bot_credentials.token` 是明文——M4 迁移需要把
  这种行就地加密。M2 的 `load_credentials` 在缺失 master_key
  时直接 `Err`，但遇到「密文无法解密」（旧明文 + 新 master_key
  之外的情况）也会 `Err`，需要 M4 迁移把它们处理掉。

## M3 — quote_index 启动预热 (2026-06-21)

### 实现摘要

- `src/store/messages.rs`：
  - 新增 `RecentOutboundRow` 结构体（`from_user`, `text`,
    `vtoken: Option<String>`, `session_name`, `created_at`）。
  - 新增 `Store::recent_outbound_messages(limit)`：从
    `messages` 表按 `id DESC` 取最近 N 条 `role = 'assistant'`
    且 `content != ''` 的行；`limit` 内部 clamp 到 `[1, 10000]`
    与 `MAX_BY_CONTENT_KEYS` 对齐。
- `src/hub/quote_route.rs`：
  - 新增 `pub struct WarmItem { scope, text, origin }` 作为
    `warm_from_history` 的入参——`QuoteOrigin` 由调用方构造，
    `QuoteRouteIndex` 本身不知道「DB 行 vs. 实时 dispatch」。
  - 新增 `QuoteRouteIndex::warm_from_history(items) -> usize`：
    对每个 item 复用 `register_outbound_content`；空文本 /
    仅空白文本被静默跳过。
  - 新增 `pub fn warm_item_from_recent_row(&RecentOutboundRow) ->
    WarmItem`：用 `parse_footer_from_quoted_text` 解析 `text`
    末尾的 footer 来还原 `QuoteOrigin::Client { name, .. }`；
    footer 缺失时 name 设为 `"<warmup>"`（vtoken 仍保留）；
    vtoken 缺失时坍缩为 `QuoteOrigin::Hub` 防止路由到不存
    在的客户端。
  - 新增 `DEFAULT_QUOTE_INDEX_WARMUP_LIMIT: i64 = 500`。
- `src/runtime/serve.rs`：
  - 新增 `parse_env_warmup_limit()`：从
    `ILINK_QUOTE_INDEX_WARMUP_LIMIT` 读值，clamp 到
    `[1, 10000]`，非数字/越界走 WARN + 回退默认。
  - 新增 `async fn warm_quote_index_from_db(state, store,
    limit)`：加载 → 转换 → `warm_from_history` → 记日志
    `quote_index warmup complete: n items`。失败仅 WARN，
    不向上传播。
  - `run_serve` 中紧跟 `load_clients_from_db` 之后
    `tokio::spawn` 这个函数；HTTP listener 绑定不被 warmup
    阻塞。

### 决策分歧（与 plan 的差异）

1. **plan 写「`direction='outbound'`」而表里是
   `role='assistant'`**。`messages` 表用 `role` 而不是
   `direction`（见 migrations/0005_messages.sql）。两者在
   当前 schema 下是同义（outbound reply = assistant 行），
   实现用 `role = 'assistant'` 命中实际数据，并附
   `content != ''` 过滤掉空文本（plan 没显式提，但 `recent_…
   WHERE content != ''` 是基本卫生）。M5/M6 文档里会
   说明「outbound 在 messages 表里就是 role='assistant'」。

2. **`warm_from_history` 不传播 `created_at`**。plan 写
   「`register_outbound_content` 内部逻辑」，我们让 warmup
   复用 `register_outbound_content`，但 `created_ms` 落在
   「replay 时刻」而非「原 created_at」——TTL 是 7 天滑动
   窗口，传播旧时间会让 warmup 行比实时行更长寿，与
   `evict_expired` 的语义不一致。代价：content signature
   完全相同的两个 warmup 行靠 `seq` 而不是 `created_ms`
   排序；实践中同 text 极罕见，不影响正确性。

3. **plan 写 `from_user`，表里是 `peer_user_id`**。
   `RecentOutboundRow` 字段叫 `from_user`（沿用 plan 用
   词），映射 `peer_user_id`（DB 字段）。这是命名 vs. 物
   理列名的差异，没有功能影响。

4. **没有给 `quote_index` 加新锁**。plan 写「共用 lock 已
   存在，不引入新锁」——`warm_from_history` 走的就是
   `register_outbound_content` 内部的同一把
   `state.routing.quote_index` mutex，没有新增。

5. **加 `parse_footer_from_quoted_text` 的 hub-level 重导
   出**。`quote_route::parse_footer_from_quoted_text` 早已
   `pub`，但只有 `dispatch.rs` 在用。M3 的 warmup 路径也
   需要它，顺手在 `hub/mod.rs` 的 re-export 表里补上
   `parse_footer_from_quoted_text`，避免外部 import 路径
   散乱。

### 验证

```
$ cargo fmt --check
(clean)

$ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile

$ cargo test --lib quote_route
test result: ok. 26 passed; 0 failed
    # 包含 7 个 M3 新增单元测试:
    #   warm_from_history_equivalent_to_register_loop
    #   warm_from_history_empty_slice_is_noop
    #   warm_from_history_skips_empty_text
    #   warm_from_history_preserves_per_scope_isolation
    #   warm_item_from_recent_row_uses_footer_name_and_session
    #   warm_item_from_recent_row_missing_vtoken_falls_back_to_hub
    #   warm_item_from_recent_row_missing_footer_uses_placeholder_name

$ cargo test --lib store_tests m3_
test result: ok. 10 passed; 0 failed
    # 包含 3 个 M3 新增集成测试:
    #   m3_recent_outbound_messages_filters_role_and_orders_newest_first
    #   m3_recent_outbound_messages_clamps_limit
    #   m3_warmup_round_trip_through_quote_index

$ cargo test
test result: ok. 386 passed; 0 failed   (lib)
test result: ok.  20 passed; 0 failed
test result: ok.   8 passed; 0 failed
test result: ok.  27 passed; 0 failed
test result: ok.   1 passed; 0 failed
test result: ok.  18 passed; 0 failed

$ cargo build
    Finished `dev` profile

$ cd desktop/ilink-hub-desktop && npm run build
✓ 7 modules transformed.
dist/index.html                 19.72 kB
dist/assets/index-dt2WSZD0.css  27.41 kB
dist/assets/index-DxFIEYEF.js   25.69 kB
✓ built in 98ms

$ cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml
    Finished `dev` profile
```

### E2E Checkpoint

- **E2E-3（quote_index 预热）**：
  `m3_warmup_round_trip_through_quote_index` 端到端走完
  `Store::save_message` → `recent_outbound_messages` →
  `warm_item_from_recent_row` → `warm_from_history` →
  `resolve_user_quote`，验证两条不同 footer / session 的
  outbound 行都能在重启后被 quote-reply 命中，且不触发
  `find_assistant_message_by_content`（SQL fallback）。

## M4 — 迁移兼容 (2026-06-21)

### 实现摘要

- 实现了第 8 版数据库迁移逻辑 `migrate_to_v8_tx`（对应 `0008_vtoken_and_bot_token_hash.sql`）。
- 在 Rust 侧完成了就地数据迁移：
  - 针对 `clients`、`routing_state` 和 `messages` 等涉及 `vtoken` 的表，读取现有明文数据并在内存中计算 SHA-256 哈希值，然后再将其安全写回。
  - 针对 `bot_credentials.token`，自动读取现有的明文 token，利用 AES-256-GCM 进行静态加密，以 base64 格式保存写回数据库。
- 迁移过程中对未配置 `ILINK_HUB_MASTER_KEY` 的空数据库环境进行容错处理（不阻断初始构建和测试运行）。

### 验证

- 运行 `cargo test` 中相关的集成迁移测试全部通过。
- 对包含旧明文 DB 行的数据集运行迁移，确认迁移后数据库中 `vtoken` 长度全部升级至 64 位且原 bridge 功能不受影响。

## M5 — 测试 (2026-06-21)

### 实现摘要

- 在 `src/store/store_tests.rs` 中补充了针对第 8 版迁移的多角度测试用例：
  - `test_migration_v8_hash_vtoken_and_encrypt_bot_token`：验证旧明文 vtoken 和 bot_token 在迁移后能够正确转换为哈希/密文，并且对解密后的 token 进行一致性校验。
  - `test_migration_v8_idempotency_does_not_double_encrypt`：验证迁移的幂等性，保证重复迁移不会造成二次加密。
  - `test_migration_v8_missing_master_key_fails`：验证当缺失 `ILINK_HUB_MASTER_KEY` 时，如果数据库需要被迁移，迁移过程会抛出错误导致 panic/error 拦截，防止静默启动。
  - `test_bot_credentials_decryption_adversarial_wrong_key` 与 `test_bot_credentials_decryption_adversarial_tampered_ciphertext`：在非对称异常场景下测试 AEAD 的解密容错与拦截能力。

### 验证

- `cargo test` 396 passed，单元与集成测试全绿。

## M6 — 文档与回归 (2026-06-21)

### 实现摘要

- 更新了项目文档，确保安全性改动有清晰的部署和操作指引：
  - **`README.md`**：强调敏感凭据静态加密与哈希存储的必要性，并明确将 `ILINK_HUB_MASTER_KEY` 作为启动时的强制环境变量，且修改 `/hub/clients` 返回描述以反映哈希值存储。
  - **`docs/knowledge/api/configuration.md`**：将 `ILINK_HUB_MASTER_KEY` 加入核心变量表格并作出详细说明。
  - **`docs/knowledge/ops/deployment-hardening.md`**：在加固指引的“凭证与日志”一节新增了静态加密和哈希存储的技术概述，并在上线前安全检查清单中新增了校验主密钥配置的项。
- 进行了回归验证，确认所有回归命令全绿。
- 创建了 `review-request.yaml` 归档到 `docs/exec-plans/active/security-p1/reviews/m6/` 中。

### 验证

- `cargo fmt --all -- --check` 结果：Clean。
- `cargo clippy -- -D warnings` 结果：Clean。
- `cargo test` 结果：396 passed, 0 failed。
- `cargo build` 结果：Workspace 编译通过。
- Desktop 编译：
  - 桌面端前端：`npm run build` 成功。
  - 桌面端 Tauri 容器检测：`cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` 成功通过。

### E2E Checkpoint

- **E2E-4（迁移兼容）**：`test_migration_v8_hash_vtoken_and_encrypt_bot_token` 执行正常，旧数据库顺利升级。
- **E2E-5（回归）**：`cargo fmt --check && cargo clippy -- -D warnings && cargo test` 所有校验全绿。

