# 目标

为 ilink-hub Axum 服务器补全两个安全措施，这两条在 docs/TODO.md 里标为 done，但代码里根本没实现：

1. **SEC-010: HTTP Body 大小限制**
   - 在 `build_router`（src/server/mod.rs）加全局 `DefaultBodyLimit::max(256 * 1024)`（256 KB）
   - 对确有需要上传大内容的路由（如 /hub/sendmessage 的消息体）单独放开到 4 MB
   - 补充对应单元测试（超大 body 返回 413）

2. **SEC-007: /metrics 端点需鉴权**
   - 当前 `/metrics` 暴露 client 名称、队列深度、消息计数，无任何鉴权
   - 对 `metrics` handler 添加 `check_admin_auth`（参考 routes.rs 里其他 admin 路由的做法）
   - 补充测试：无 token 返回 401，有 token 返回 200

## 完成标准

- [ ] `cargo clippy -- -D warnings` 零警告
- [ ] `cargo test` 全部通过
- [ ] `cargo build` 成功
- [ ] 新测试覆盖 body limit 413 场景和 metrics 401/200 场景
- [ ] docs/TODO.md SEC-010 和 SEC-007 条目确认补全说明

## 非目标

- 不改变其他路由的行为
- 不修改 /metrics 的数据格式
- 不引入新依赖（DefaultBodyLimit 已在 axum::extract，check_admin_auth 已存在）
