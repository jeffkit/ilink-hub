# Feature: cors-configurable-origins

## 目标

将 `src/server/mod.rs` 中硬编码的 `CorsLayer::permissive()` 改为可通过环境变量 `ILINK_CORS_ORIGINS` 配置的来源白名单，消除内网部署时恶意页面跨域探测 bot API 的风险。

## 完成标准

- [ ] `ILINK_CORS_ORIGINS` 未设置时行为与现在一致（保持 permissive，并打印 WARN 日志提示）
- [ ] `ILINK_CORS_ORIGINS=https://a.com,https://b.com` 时，CORS 只允许列表中的来源
- [ ] 非法来源格式时启动报错（不静默忽略）
- [ ] `cargo test` 全部通过
- [ ] `cargo clippy -- -D warnings` 零警告
- [ ] `cargo build` 成功
- [ ] README 或 docs/ 中有关于 ILINK_CORS_ORIGINS 的说明

## 非目标

- 不改变 admin / hub 管理端点的 CORS 行为（它们已无 CORS header）
- 不引入新的 HTTP 中间件框架依赖
- 不修改现有鉴权逻辑

## 背景 / 约束

- 文件：`src/server/mod.rs` 约第 23 行 `CorsLayer::permissive()`
- 使用 `tower-http` 已有的 `CorsLayer::new().allow_origin(list)` API
- 环境变量格式：逗号分隔的完整 origin（含 scheme，如 `https://example.com`）
