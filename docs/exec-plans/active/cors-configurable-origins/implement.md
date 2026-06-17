# 实现记录

## M1: 核心实现 ✅ 完成 (2026-06-17)

**变更文件：** `src/server/mod.rs`

**改动摘要：**
- 新增 `parse_origins()` 函数：从逗号分隔字符串解析 HTTP origin 列表
- 新增 `build_cors_layer()` 函数：读取 `ILINK_CORS_ORIGINS` 环境变量，设置时构建受限 CORS 层，未设置时回退到 permissive
- 替换 `CorsLayer::permissive()` 为 `build_cors_layer()`
- 新增 6 个单元测试覆盖：单/多起源、空白 trim、空字符串、控制字符拒绝

**验证状态：**
| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | 241 passed |
| `cargo build` | pass |
| `npm run build` (desktop) | pass |
| `cargo check --manifest-path desktop/.../Cargo.toml` | pass |
