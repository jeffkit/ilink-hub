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

## M2: 边界处理 ✅ 完成 (2026-06-17)

**变更文件：** `src/server/mod.rs`

**改动摘要：**
- `parse_origins()`: 新增 scheme 检查 — 不含 `://` 的 origin（如 `bad-origin`、`*`、`null`）直接 panic
- `build_cors_layer()`: fallback 分支添加 `tracing::warn!` 日志，提示未设置 ILINK_CORS_ORIGINS 时回退到 permissive CORS
- 测试更新: `origins_rejects_wildcard`、`origins_rejects_null_origin` 改为 `should_panic(expected = "without scheme")`
- 新增测试: `origins_rejects_no_scheme`、`origins_rejects_mixed_with_bad_origin`

**验证状态：**
| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | 254 passed |
| `cargo build` | pass |
| `npm run build` (desktop) | pass |
| `cargo check --manifest-path desktop/.../Cargo.toml` | pass |
