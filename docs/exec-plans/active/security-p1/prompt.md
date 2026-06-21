# security-p1

## 目标

修复 ilink-hub 中三个高优先级安全与可靠性问题，让"DB 文件泄露 ≠ 所有客户端凭据泄露"，并让 Hub 重启后 quote routing 不需要等待消息重建索引。

具体改动：

1. **vtoken 哈希存储**：DB 中 `clients.vtoken` 改为存 SHA-256(vtoken)，内存 registry 同步改用 hash 做 key；现有 vtoken 生成逻辑和格式不变，只是落盘时不再保存原文。
2. **bot_token 静态加密**：`bot_credentials.token` 落盘前用 AES-256-GCM 加密（密钥来自启动时环境变量 `ILINK_HUB_MASTER_KEY` 或派生文件），读取时解密；DB 文件泄露后攻击者无法直接复用 token。
3. **quote_index 启动预热**：Hub 启动时从 DB 读取最近 N 条（默认 500 条）outbound 消息，重建内存 `QuoteRouteIndex`，避免重启后 quote routing 全走慢路径 SQL LIKE 查询。

## 完成标准

每条可通过命令或具体场景独立验证：

1. **vtoken hash**：`SELECT vtoken FROM clients LIMIT 1` 返回的值是 64 位 hex 字符串（SHA-256），不是 `vhub_` 开头的原文；`cargo test` 中 vtoken 注册/查找路径测试全绿。
2. **bot_token 加密**：`SELECT token FROM bot_credentials LIMIT 1` 返回的值不可直接用于 iLink API 调用（是密文）；设置 `ILINK_HUB_MASTER_KEY` 后服务启动、登录、消息收发功能正常；不设 `ILINK_HUB_MASTER_KEY` 时服务拒绝启动并打印明确错误。
3. **quote_index 预热**：Hub 重启后首条 quote reply 不触发 SQL LIKE fallback（可通过日志中无 `"v7 index absent"` 或 `"cold"` 关键字验证）；`cargo test` 中新增预热路径的单元测试通过。
4. **回归**：`cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test` 全部通过。
5. **迁移兼容**：已有 DB（含明文 vtoken 和 bot_token）可通过迁移脚本或启动时自动迁移升级，不需要手动重新注册。

## 硬约束

- vtoken 原文**只在 `register` 响应时返回一次**，之后任何地方只存/比 hash，不得出现原文落盘。
- `ILINK_HUB_MASTER_KEY` 缺失时**必须拒绝启动**，不得 silently 降级为明文存储。
- 加密方案使用 `ring` crate（项目已引入），不引入新的加密依赖。
- 预热查询**不得阻塞 Hub 启动**：用 `tokio::spawn` 异步预热，启动完成后索引逐步就绪。
- 不改变任何 HTTP API 的 request/response schema。

## 非目标

- 不做 vtoken HMAC 签名（签名防篡改，hash 存储防泄露，两者独立；本次只做防泄露）。
- 不做 bot_token 的 key rotation 机制（后续 security-p2）。
- 不做 quote_index 持久化到 SQLite（预热已够用，持久化带来写入同步成本）。
- 不做任何 HTTP API 的 breaking change。
- 不做 Postgres/MySQL 的迁移脚本（项目主要用 SQLite，其他方言迁移留后续）。
