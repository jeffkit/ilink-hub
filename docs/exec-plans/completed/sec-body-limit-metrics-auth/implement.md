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

---

## M2: 实现 SEC-010 Body Limit — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：实现 SEC-010 Body Limit (256 KB 全局限制与 /ilink/bot/sendmessage 4 MB 放宽限制)，并编写边界单元测试。
- **审查请求**：[reviews/m2/review-request.yaml](./reviews/m2/review-request.yaml)

### 关键改动

- 在 `src/server/mod.rs` 中引入 `axum::extract::DefaultBodyLimit`。
- 在全局 Router 挂载 `.layer(DefaultBodyLimit::max(256 * 1024))`，将全局 payload 大小限制设为 256 KB。
- 针对 `/ilink/bot/sendmessage` 路由单独设置 `.layer(DefaultBodyLimit::max(4 * 1024 * 1024))`，将限制放宽至 4 MB。
- 在 `tests/breaking_changes.rs` 编写 `test_body_limit_global` 与 `test_body_limit_sendmessage_override` 两个单元测试，全面校验：
  - 全局 256 KB 边界：刚好 256 KB 可接受，多 1 字节返回 413 Payload Too Large。
  - sendmessage 4 MB 边界：刚好 4 MB 可接受，多 1 字节返回 413 Payload Too Large，而在 256 KB + 1 字节时可顺利通过（不报 413）。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: 59723a2

---

## M3: 实现 SEC-007 Metrics Auth — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：实现 SEC-007 Metrics Auth（/metrics 接口加 admin 鉴权），并编写配套单元与集成测试。
- **审查请求**：[reviews/m3/review-request.yaml](./reviews/m3/review-request.yaml)

### 关键改动

- 修改 `src/server/routes.rs` 的 `metrics` handler，增加 `headers: HeaderMap` 参数，并在入口添加 `if !check_admin_auth(&headers) { return (StatusCode::UNAUTHORIZED, "Unauthorized".into()); }` 阻断未授权请求。
- 在 `tests/breaking_changes.rs` 编写 `metrics_requires_auth_when_no_token_configured` 单元测试，校验在没有配置 admin token 时，直接请求 `/metrics` 能够返回 401。
- 在 `tests/e2e_wechat_simulation.rs` 编写 `test_metrics_endpoint_auth` 集成测试，检验 E2E 模式下，分别输入无 token、错误 token（均返回 401）和正确 token（返回 200 及监控指标数据）时接口的行为。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

Commit: ea4ee57

---

## M4: 单元测试 - Body Limit 413 — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：单元测试 - Body Limit 413 边界条件及 sendmessage 放宽限制验证。
- **审查请求**：[reviews/m4/review-request.yaml](./reviews/m4/review-request.yaml)

### 关键改动

- 验证并执行 `tests/breaking_changes.rs` 中的 `test_body_limit_global` 与 `test_body_limit_sendmessage_override` 两个单元测试：
  - 验证全局 256 KB 边界：刚好 256 KB 可接受，多 1 字节返回 413 Payload Too Large。
  - 验证 sendmessage 4 MB 边界：刚好 4 MB 可接受，多 1 字节返回 413 Payload Too Large，而在 256 KB + 1 字节时可顺利通过（不报 413）。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: 81ab0e4

---

## M5: 单元测试 - Metrics 401/200 — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：单元测试 - Metrics 401/200 验证。
- **审查请求**：[reviews/m5/review-request.yaml](./reviews/m5/review-request.yaml)

### 关键改动

- 新建 `tests/metrics_auth_tests.rs`，包含 `test_metrics_auth_with_configured_token` 测试用例，全面覆盖了配置了 admin token 时访问 `/metrics` 的行为：
  - 无 token 访问 -> 返回 401 Unauthorized。
  - 错误 token 访问 -> 返回 401 Unauthorized。
  - 正确 token 访问 -> 返回 200 OK 并能获取指标数据。
- 通过使用独立的 integration test 目标文件，充分利用 Cargo 多进程运行机制，安全隔离了鉴权 `OnceLock` 变量的读写操作，避免了并行执行时的测试竞争或干扰。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: 099971e

---

## M6: 质量门禁 — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：执行六项质量门禁命令（fmt、clippy、test、build 以及桌面端编译和 Tauri check），保证所有检查全绿通过。
- **审查请求**：[reviews/m6/review-request.yaml](./reviews/m6/review-request.yaml)

### 关键改动

- 执行并验证了以下六项质量门禁命令，均无任何错误或警告：
  - `cargo fmt --check`
  - `cargo clippy -- -D warnings`
  - `cargo test`
  - `cargo build`
  - `cd desktop/ilink-hub-desktop && { [ -e node_modules ] || ln -s /Users/kongjie/projects/ilink-hub/desktop/ilink-hub-desktop/node_modules node_modules; } && npm run build`
  - `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml`

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: 3407250

---

## M7: 文档同步 — done (2026-06-17)

### 状态
- **状态**：done
- **范围**：文档同步工作。标记 docs/TODO.md 中的 SEC-007 和 SEC-010 状态并补充修复的 commit 详情。
- **审查请求**：[reviews/m7/review-request.yaml](./reviews/m7/review-request.yaml)

### 关键改动

- 在 `docs/TODO.md` 中：
  - 将 SEC-007 和 SEC-010 追加到已完成项列表中。
  - 将 SEC-007 的状态更新为 `done (commit: ea4ee57)`。
  - 将 SEC-010 的状态更新为 `done (commit: f7f8110)`。
  - 在“已完成的修复记录”表格中追加 SEC-007 和 SEC-010 的修复简述。
- 确认了 `docs/DOC_CODE_MAP.md` 无需新增条目（项目无此文件）。
- 创建了 `docs/exec-plans/active/sec-body-limit-metrics-auth/reviews/m7/review-request.yaml` 审查文件。

### 验证结果

| 命令 | 结果 |
|------|------|
| `cargo fmt --check` | pass |
| `cargo clippy -- -D warnings` | pass |
| `cargo test` | pass |
| `cargo build` | pass |
| `cd desktop/ilink-hub-desktop && npm run build` | pass |
| `cargo check --manifest-path desktop/ilink-hub-desktop/src-tauri/Cargo.toml` | pass |

### Commit

Commit: 9e3bf78
