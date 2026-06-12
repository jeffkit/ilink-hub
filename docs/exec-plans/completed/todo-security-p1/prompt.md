修复 P1 安全修复：SEC-001, SEC-003, SEC-013

## 待修复条目

  - [SEC-001] pair_confirm TOCTOU — vtoken 劫持（CWE-362）
     文件：src/server/pairing.rs:381-386
     问题：攻击者对同一配对码并发发送两个 `POST /hub/pair/{code}/confirm` 请求（不同 name），两个请求在 `pairing.confirm()` 之前都执行完 `register_client_in_hub`，导致第一个请求抢占到 vtoken，第二个请求得到 409。操作员的客户端永远拿不到正确的 vtoken——实质上是 vtoken 命名空间的会话劫持。
     修复方向：在单个写锁内原子完成「校验 code 状态 → register_client → confirm」三步，不允许中间状态被其他请求观察到。

  - [SEC-003] getupdates 无并发限制 — 连接耗尽 DoS（CWE-307）
     文件：src/server/routes.rs:141-226, src/hub/queue.rs:394-399
     问题：持有有效 vtoken 的客户端可以开启任意数量的 60 秒长轮询连接。`PollTracker` 只打 warn 不阻断，没有每 vtoken 的并发上限。单个 vtoken 可以耗尽 Tokio worker 线程和连接池。
     修复方向：在 `getupdates` handler 中检查 `PollTracker` 的并发数，超过阈值（如 3）时立即返回 HTTP 429：   ```rust   let (count, _guard) = state.poll_tracker.enter(&vtoken);   if count > MAX_CONCURRENT_POLLS {       return (StatusCode::TOO_MANY_REQUESTS, ...).into_response();   }   ```

  - [SEC-013] pair_confirm 无认证 — 任意 vtoken 注册（CWE-284）
     文件：src/server/mod.rs:46-47, src/server/pairing.rs:362-413
     问题：`POST /hub/pair/{code}/confirm` 无认证。配对码以 INFO 级别记录在日志中（`info!(code = %code, pair_url = %pair_url)`），任何能读取 Hub 日志的人可提取活跃码，调用该端点注册任意名称客户端并获得完整 vtoken，从此接收所有路由到该后端的微信消息。
     修复方向：1. 将 pair_url 日志降为 DEBUG 级别   2. 强制要求 code 处于 `Scanned` 状态才允许 confirm（手机扫码是唯一合法触发路径）   3. 在 pair HTML 页面中嵌入 CSRF token，confirm 请求必须携带才能通过

## 完成标准
- [ ] SEC-001 修复已提交，相关测试通过
- [ ] SEC-003 修复已提交，相关测试通过
- [ ] SEC-013 修复已提交，相关测试通过
- [ ] cargo clippy 无新 warning
- [ ] cargo test 全绿

## 非目标
- 不重构不涉及上述条目的其他模块
- 不升级无关依赖