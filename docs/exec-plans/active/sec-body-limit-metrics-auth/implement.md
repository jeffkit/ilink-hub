# 实施记录：SEC-010 Body Limit + SEC-007 Metrics Auth

> 本文件按里程碑滚动记录。每个里程碑完成后追加一段：状态、关键改动、验证结果、commit 引用。

---

## M1: 调查与定位 — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：仅调查与定位；未修改任何业务代码。
- **审查请求**：[reviews/m1/review-request.yaml](./reviews/m1/review-request.yaml)

### 关键发现（事实清单）

| ID | 位置 | 结论 |
|----|------|------|
| F-1 | `src/server/mod.rs:17-61` | `build_router` 当前**没有** `DefaultBodyLimit`，也**没有** `RequestBodyLimitLayer`。bot_api 独立 CORS、admin_api 无 CORS。 |
| F-2 | handler `src/server/routes.rs:965`、route `src/server/mod.rs:53` | `/metrics` 签名是 `pub async fn metrics(State(state)) -> (StatusCode, String)`，**未读取 headers、未调用 `check_admin_auth`**。SEC-007 在 `docs/TODO.md` 标 done 是误标（prompt.md 也明确指出这是要补的）。 |
| F-3 | route `src/server/mod.rs:31`、handler `src/server/routes.rs:310` | sendmessage 实际路径是 **`/ilink/bot/sendmessage`**（不是 prompt.md 写的 `/hub/sendmessage`，prompt 是 typo）。body 是 `Json<SendMessageRequest>`，内含 `WeixinMessage.item_list: Option<Arc<Vec<MessageItem>>>`，`MessageItem.extra: serde_json::Value` 兜底所有非 text/voice 字段（含 base64 二进制），需要单独放开。 |
| F-4 | `src/server/routes.rs:57-69` | `check_admin_auth(&HeaderMap) -> bool` 已存在并使用 `subtle::ConstantTimeEq`（SEC-006）；所有 admin handler 一律 `if !check_admin_auth(&headers) { return 401; }` 早返回。**M3 直接复用，不引入新中间件。** |
| F-5 | `Cargo.toml:34` | `axum = "0.8"`，`DefaultBodyLimit` 已在 `axum::extract` 中，**无需新增依赖**。 |
| F-6 | `src/server/routes.rs:1149-1210` | 现有 `#[cfg(test)] mod shutdown_poll_tests` + `mod admin_auth_tests` 在 routes.rs 末尾；M4/M5 需要构造完整 router 才能测 413/401/200。 |

### 实施路线（M2–M7 摘要）

- **M2** — `src/server/mod.rs` build_router 全局 `.layer(DefaultBodyLimit::max(256 * 1024))`；bot_api 中 `/ilink/bot/sendmessage` 单独 `.layer(DefaultBodyLimit::max(4 * 1024 * 1024))`。
- **M3** — `metrics` 签名加 `HeaderMap` 参数，入口 `if !check_admin_auth(&headers) { return (StatusCode::UNAUTHORIZED, "Unauthorized".into()); }`。
- **M4** — 新建测试：256 KB 边界值（临界点）、`/ilink/bot/sendmessage` 4 MB 不被 413 拒绝。
- **M5** — 新建测试：无 token / 错 token → 401；正确 token → 200。
- **M6** — `cargo clippy -- -D warnings` + `cargo test` + `cargo build` 全绿。
- **M7** — `docs/TODO.md` SEC-007/SEC-010 commit 引用补齐；`docs/DOC_CODE_MAP.md` 检查是否新增条目（按用户全局规则）。

### 风险 / 注意事项

- axum 0.8 的 `DefaultBodyLimit` 超过限制时由 axum 自动返回 **413 Payload Too Large**，handler 无需主动判断。
- sendmessage 单独放开 4 MB 的依据：`item_list` 可能含 base64 image/voice/file/video，256 KB 全局上限会过紧。
- 鉴权失败统一返回 **401**（与现有 admin handler 一致）；`insecure_no_auth()` 仅作开发/测试逃生口。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

M1 未触任何业务代码，预期 baseline 全绿。详细日志与命令记录见 commit message。

### Commit

见 `git log -1` 引用（M1 调查产出提交）。
