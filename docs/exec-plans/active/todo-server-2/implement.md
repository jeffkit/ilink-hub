# 实施记录 (Implementation Record)

## 里程碑 M1: E-01 上游输入状态（typing status）错误传播修复

### 变更详情
1. **上游错误拦截**：
   - 在 `src/ilink/upstream.rs` 中的 `UpstreamClient::send_typing` 方法，添加 `.error_for_status()?` 链式调用。确保上游 HTTP 请求返回非 2xx（如 500）时能够正确抛出 `Err` 而不被静默忽略。
2. **路由错误传播**：
   - 修改 `src/server/routes.rs` 的 `sendtyping` 路由处理函数。使用 `match` 捕获 `state.upstream.send_typing` 返回的 `Result`。
   - 成功时返回 JSON `{"ret": 0}`。
   - 失败时返回 `{"ret": 500, "errmsg": "upstream error: ..."}`。
3. **集成测试验证**：
   - 在 `tests/breaking_changes.rs` 尾部添加了 `sendtyping_error_propagation_test`。
   - 启动本地 Mock Axum 服务器拦截 `/ilink/bot/sendtyping`，模拟上游正常（返回 200）和异常（返回 500）的情况，分别验证 `sendtyping` 路由返回 `ret: 0` 和 `ret: 500` 且带有错误信息的 JSON。

### 验证状态
- **fmt**: `cargo fmt --check` (已通过)
- **clippy**: `cargo clippy -- -D warnings` (已通过)
- **test**: `cargo test` (已通过)
- **build**: `cargo build` (已通过)
