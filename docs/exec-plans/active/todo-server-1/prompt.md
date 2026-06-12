修复 server 模块修复：SEC-004, SEC-006, SEC-007, SEC-009, SEC-010

## 待修复条目

  - [SEC-004] get_qrcode_status 无认证无限速 DoS（CWE-307）
     文件：src/server/pairing.rs:299-316
     问题：`GET /ilink/bot/get_qrcode_status?qrcode=<any>` 无认证，每请求最多持有 25 秒 Tokio task。攻击者可用随机 qrcode 值开启数千并发连接耗尽服务器。
     修复方向：对该端点应用 per-IP 限速（复用 relay 已有的 `RateLimiter` 模式），或在 loop 开始前验证 code 存在且处于活跃状态，不存在时立即返回 404。

  - [SEC-006] admin token 非常量时间比较（CWE-312）
     文件：src/server/routes.rs:69
     问题：在同一局域网或共享云环境中，精确计时攻击可逐字节推断 admin token。
     修复方向：使用 `subtle` crate（已是 `ed25519-dalek` 的传递依赖）进行常量时间比较：   ```rust   use subtle::ConstantTimeEq;   provided.as_bytes().ct_eq(required.as_bytes()).into()   ```

  - [SEC-007] /metrics 无认证，泄露客户端名称和消息量（CWE-284）
     文件：src/server/mod.rs:53-54, src/server/routes.rs:825-933
     问题：`GET /metrics` 无任何认证，暴露所有注册客户端名称（通过 Prometheus label）、队列深度、消息吞吐量、iLink 连接状态，可用于攻击者对部署进行指纹识别。
     修复方向：对 `/metrics` 应用 `check_admin_auth` 中间件，或通过独立内部端口暴露（推荐 Prometheus 最佳实践）。

  - [SEC-009] name/label 字段无长度校验（CWE-20）
     文件：src/server/routes.rs:109-137, register, src/server/pairing.rs:362-413, pair_confirm
     问题：`name` 和 `label` 字段无长度限制，10MB 的 name 可以被存入内存和 SQLite。Prometheus metric label 中的 client name 未做特殊字符转义（大括号、换行），可破坏 Prometheus scrape 输出。
     修复方向：在两个 handler 的入口处验证 `name.len() <= 64`、`label.len() <= 256`，超出返回 HTTP 400；Prometheus 输出中对 client name 进行转义。

  - [SEC-010] 所有路由无 HTTP body 大小限制（CWE-400）
     文件：src/server/mod.rs:17-61, RequestBodyLimitLayer
     问题：Axum 默认 body 限制为 2MB，但未显式配置 `DefaultBodyLimit`。`sendmessage` 接受含 `item_list`（嵌套 voice、text、binary payload）的请求体，relay 的 `body: String` 字段不做长度检查，可被恶意 relay 推送超大 body 到 Hub 本地 HTTP 栈。
     修复方向：在 `build_router` 中加入显式限制：   ```rust   .layer(DefaultBodyLimit::max(256 * 1024)) // 256KB 全局上限   ```   可对特定路由通过 `.layer(DefaultBodyLimit::disable())` 单独放开。

## 完成标准
- [ ] SEC-004 修复已提交，相关测试通过
- [ ] SEC-006 修复已提交，相关测试通过
- [ ] SEC-007 修复已提交，相关测试通过
- [ ] SEC-009 修复已提交，相关测试通过
- [ ] SEC-010 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖