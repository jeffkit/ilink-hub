# Security P1 Plan

## 范围

涉及 ilink-hub 单仓改动，覆盖存储层（clients / bot_credentials / messages）、运行时（serve 启动路径、密钥加载、quote_index 预热）、客户端 registry 内部表示，以及迁移与测试。HTTP API 保持不变。

## 设计

### M1 — vtoken 哈希存储

- 在 `src/store/clients.rs` 与 `src/hub/registry.rs` 之间新增一个 thin helper（`src/hub/vtoken_hash.rs`），提供 `hash_vtoken(plain: &str) -> String`（SHA-256，输出 64 位 hex）与 `is_vtoken_hash(s: &str) -> bool`。
- `Store::upsert_client` / `update_client_by_vtoken` / `set_route` / `clear_routes_for_vtoken` / `touch_client` 等所有写入与查询路径在拿到明文 vtoken 时先 `hash_vtoken`，再绑定到 SQL；`list_clients` 与 `list_routes` 返回的 `vtoken` 已经是 hash。
- `ClientRegistry` 内部 `by_vtoken` 的 key 与 `ClientInfo::vtoken` 字段全部改存 hash；`register` 返回值（响应里给客户端的）仍是明文，但只通过 `register` 入口返回一次，DB / 内存中都不再有明文。
- `get_by_vtoken` / `mark_online` / `mark_offline` 等查找点：调用方在传入明文 vtoken 时先 hash；如果调用方已经是 hash，则直接使用。
- 启动时 `load_clients_from_db` 读到 DB 中的 hash 直接灌进 registry（无需再次 hash）；`register` 路径上生成明文后立刻 hash 入库并写入内存。

### M2 — bot_token 静态加密

- 新增 `src/runtime/crypto.rs`，使用 `ring::aead::AES_256_GCM`：
  - `encrypt_token(plain: &str, key: &Key) -> String`（输出 base64：`nonce(12) || ct || tag(16)`）
  - `decrypt_token(blob: &str, key: &Key) -> Result<String>`
  - `load_or_derive_master_key() -> Result<Key>`：从环境变量 `ILINK_HUB_MASTER_KEY`（32 字节的 base64 或 hex 形式）读取；缺失或格式错误直接返回错误并让进程退出。
- `Store::save_credentials` / `load_credentials` 改为：写时先 encrypt 再 bind，读到密文后 decrypt；调用方不感知加密。
- `src/runtime/serve.rs::run_serve` 最早期（创建 `Store` 之前或之后、`HubState` 之前）调用 `load_or_derive_master_key()`，拿到 `Arc<Key>` 并存入 `ServeOptions` 或一个 `CryptoContext`。
- 不设 `ILINK_HUB_MASTER_KEY` 时 `run_serve` 立即返回错误并 `tracing::error!` 打印明确信息（不得 silently fallback）。

### M3 — quote_index 启动预热

- 在 `Store` 上新增 `async fn recent_outbound_messages(limit: i64) -> Result<Vec<(from_user, text, scope, ts)>>`，从 `messages` 表按 `created_at` DESC 取最近 N 条（默认 500，命名常量 `QUOTE_INDEX_WARMUP_LIMIT`），只取 `direction='outbound'`。
- 在 `QuoteRouteIndex` 上新增 `fn warm_from_history(items: &[WarmItem])`，复用 `register_outbound_content` 内部逻辑，把每条消息按 `direction=outbound` 注册进索引。
- `src/runtime/serve.rs` 中，紧跟现有 `load_clients_from_db` 之后，新增 `tokio::spawn(async move { ... })` 调用 `warm_quote_index_from_db(state, store, limit)`：拿到结果后写日志 `quote_index warmup complete: n items`，失败仅 `warn`，绝不阻塞启动。
- 关键不变量：预热期间到来的 quote reply 走 SQL LIKE fallback（已有），预热完成后自动切到内存路径；`register_outbound_content` 与 fallback 路径共用的 lock 已存在，不引入新锁。

### M4 — 迁移兼容

- 新增迁移文件 `migrations/0008_vtoken_and_bot_token_hash.sql`（仅 SQLite 方言；Postgres 走 `store_tests` 路径即可，后续 security-p2 再补）：
  - 对 `clients.vtoken` 与 `bot_credentials.token` 应用一次性 inline 转换：
    - `clients.vtoken`：若长度≠64 或不是 hex，则 `UPDATE clients SET vtoken = sha256_hex(vtoken)`。SQLite 用 `substr` + `hex` + 预计算方式实现，或在 Rust migration 里执行 SQL：`UPDATE clients SET vtoken = (SELECT lower(hex(randomblob(4))) || ... )` 之类的方案不通用；最终方案为：迁移里检测 `typeof/length(vtoken)=32 OR vtoken GLOB '*[!0-9a-f]*'`，需要 hash 的批量 hash 到内存再写回。落地形式为「迁移纯 SQL 不可行 → 在 Rust 中执行：在 `migrations.rs` 增加一个 v8 步骤，使用 `ALTER TABLE` + 读所有行、内存 SHA-256、写回」。
  - `bot_credentials.token`：若不是 base64 密文（启发式：长度 > 80 且含 `/` 或 `+`），则 encrypt 后写回；逻辑在 Rust 迁移中执行。
- 检测逻辑：先用 `SELECT vtoken FROM clients LIMIT 1` 看是否已为 64 位 hex；若是则视为已迁移，跳过。
- 迁移失败时回滚事务，已迁移的行不残留半状态。

### M5 — 测试

- 新增 `src/store/store_tests.rs`（或扩展）覆盖：
  - vtoken 落盘为 hash；`get_by_vtoken(明文)` 命中；明文与 hash 互不串台。
  - 旧库（手工塞一行明文 vtoken）通过 `migrations::run` 后变成 hash。
  - bot_token 加密落盘；`load_credentials` 拿到原文；缺 `ILINK_HUB_MASTER_KEY` 时迁移启动直接 panic/error。
  - `QuoteRouteIndex::warm_from_history` 等价于手工 `register_outbound_content` N 次；预热后 `resolve_quote` 不走 SQL fallback（用 spy / log assertion）。

### M6 — 文档与回归

- 更新 `README.md` / `docs/` 中提及 vtoken / bot_token 的段落，强调「不存明文」「必须设 `ILINK_HUB_MASTER_KEY`」。
- 现有 `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test` 全绿。

## 验证命令

每个里程碑对应一条或多条可直接运行的验证：

- M1：`sqlite3 ilink-hub.db "SELECT vtoken FROM clients LIMIT 1;"` 应当返回 64 位 hex（运行任意一次 register 后再查）。`cargo test -p ilink-hub --lib registry` 与 `cargo test -p ilink-hub --lib store::clients` 全绿。
- M2：
  - 未设置 `ILINK_HUB_MASTER_KEY` 时 `ilink-hub serve` 进程退出码非 0，stderr 含 `ILINK_HUB_MASTER_KEY is required`。
  - `sqlite3 ilink-hub.db "SELECT token FROM bot_credentials LIMIT 1;"` 返回的是 base64 密文（不是明文 token）。
  - 设置 `ILINK_HUB_MASTER_KEY` 后启动 + QR 登录 + 收发消息功能不回归。
- M3：
  - 启动 Hub → 触发一次 outbound 消息 → kill → 重启 → 立即触发一条 quote reply。
  - 验证日志中无 `v7 index absent` / `cold` 关键字（grep 启动日志），且 `cargo test -p ilink-hub --lib quote_route::warm_from_history` 通过。
- M4：拷贝一份现有含明文 vtoken 的 `ilink-hub.db` 到临时目录，启动 Hub，验证 `SELECT vtoken FROM clients` 全部变为 64 位 hex，且现有 bridge 用原 vtoken 仍能正常登录（因为 hash 入库等价于「该 vtoken 的 hash 即注册身份」，bridge 在 register 时拿到的就是明文 → 同一明文 → 同一 hash）。
- M5 / M6：`cargo fmt --check && cargo clippy -- -D warnings && cargo test`。
- 端到端：参考 M3 流程跑一遍 `ilink-hub serve` + `ilink-hub-bridge` + 触发 quote。

## E2E Checkpoint 标记

下列 checkpoint 在执行时必须在 commit message 与 status.md 中显式打勾，并附上验证命令的原始输出片段：

- **E2E-1（vtoken hash 落盘）**：用 M1 的 sqlite 查询命令抓取落盘值并贴入 status.md。
- **E2E-2（bot_token 加密 + 缺 master_key 拒绝启动）**：未设 master_key 启动失败的 stderr 与密文 SELECT 输出贴入 status.md。
- **E2E-3（quote_index 预热）**：启动日志中无 `v7 index absent` / `cold`，warmup 行显示 `n items`（n≥已发送消息数）。
- **E2E-4（迁移兼容）**：旧 DB 升级后所有 `clients.vtoken` 长度 = 64，且无 bridge 重新注册步骤。
- **E2E-5（回归）**：`cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test` 三个命令最终输出贴入 status.md。

## 风险

- 哈希方案一旦上线，旧明文 DB 行若未跑迁移会触发所有 bridge 重新登录；迁移步骤必须随首次升级配套执行。
- `ILINK_HUB_MASTER_KEY` 缺失拒绝启动是个 breaking change：现有部署需要在升级前配置好，否则 hub 无法启动；在 README 中明确标注。
- 预热期间 quote reply 仍走 SQL LIKE，N=500 是合理默认，但若用户消息量极大且 quote reply 集中在冷启动窗口内仍会感知延迟；可观察一段时间后调参。
- 迁移中 Rust 侧的循环 UPDATE 在大表上较慢；迁移仅在升级时一次性发生，且 Hub 启动阻塞到迁移完成，符合预期。
- 加密方案使用 `ring` AES-256-GCM，nonce 由 `ring::aead::NonceSequence` 或每次随机 12 字节生成；解密失败需明确报错（避免 silent fallback 到空字符串）。