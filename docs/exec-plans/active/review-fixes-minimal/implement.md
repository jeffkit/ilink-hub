# Implement Log: review-fixes-minimal

## M1 — 安全文档与 Docker 示例完善 ✅

**完成时间**: 2026-06-17

### 变更摘要

| 文件 | 变更 |
|------|------|
| `README.md` | Docker Compose 示例添加 `ILINK_ADMIN_TOKEN` 注释；Admin auth 段落重写，标注为必填，添加 `ILINK_ADMIN_INSECURE_NO_AUTH=true` 安全 WARNING |
| `deploy/docker-compose.example.yml` | 新建独立 Docker Compose 部署示例，`ILINK_ADMIN_TOKEN` 为必填项 |

### 验证结果

- [x] `grep ILINK_ADMIN_TOKEN / ILINK_ADMIN_INSECURE_NO_AUTH` — 确认变更存在（README.md 7处 + deploy 3处）
- [x] `cargo fmt --check` — 零差异
- [x] `cargo clippy -- -D warnings` — 零警告
- [x] `cargo test` — 235 passed, 0 failed
- [x] `cargo build` — 成功
- [x] `npm run build` (desktop frontend) — 成功
- [x] `cargo check` (desktop tauri) — 成功
- [ ] M3 待执行
- [ ] M4 待执行

## M2 — AUTH_ERROR_KEYWORDS 常量提取 ✅

**完成时间**: 2026-06-17

### 变更摘要

| 文件 | 变更 |
|------|------|
| `src/bridge/mod.rs` | 新增 `const AUTH_ERROR_KEYWORDS: &[&str]`（12 个关键词）；`handle_one_message` 和 `dry_run_profile` 均使用该常量替代本地 `keywords` 数组 |

### 验证结果

- [x] `grep AUTH_ERROR_KEYWORDS src/bridge/mod.rs` — 3 处引用（1 定义 + 2 使用）
- [x] `cargo fmt --check` — 零差异
- [x] `cargo clippy -- -D warnings` — 零警告
- [x] `cargo test` — 293 passed, 0 failed
- [x] `cargo build` — 成功
- [x] `npm run build` (desktop frontend) — 成功
- [x] `cargo check` (desktop tauri) — 成功
