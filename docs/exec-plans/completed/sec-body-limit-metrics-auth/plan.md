# 实施计划：SEC-010 Body Limit + SEC-007 Metrics Auth

## 里程碑

### M1: 调查与定位
- 阅读 `src/server/mod.rs`，定位 `build_router` 函数
- 找到 `/metrics` handler，确认是否在 `routes.rs`
- 找到 `check_admin_auth` 现有实现和 `check_admin_auth` 用法样例
- 定位 `/hub/sendmessage` handler，确认 body 大小需求

**验证命令**:
```bash
grep -n "build_router\|metrics\|sendmessage\|check_admin_auth" src/server/mod.rs src/server/routes.rs
```

---

### M2: 实现 SEC-010 Body Limit
- 在 `build_router` 中使用 `DefaultBodyLimit::max(256 * 1024)` 包裹 router（或在 axum::Router 层设置）
- 为 `/hub/sendmessage` 单独放开到 4 MB（用 `.layer(DefaultBodyLimit::max(4 * 1024 * 1024))`）
- 检查 `Cargo.toml` 确认 `axum::extract::DefaultBodyLimit` 可用（无新依赖）

**验证命令**:
```bash
cargo build
grep -n "DefaultBodyLimit" src/server/mod.rs
```

---

### M3: 实现 SEC-007 Metrics Auth
- 修改 `metrics` handler，添加 `check_admin_auth` 调用
- 调整函数签名让其返回 `Result<...>` 或在内部校验后返回 401
- 与其他 admin 路由保持一致的鉴权风格

**验证命令**:
```bash
cargo build
grep -n "check_admin_auth" src/server/routes.rs
```

---

### M4: 单元测试 - Body Limit 413
- 在 routes 测试模块添加测试：发送超过 256 KB 的 body 到任意普通路由，断言返回 413
- 对 `/hub/sendmessage` 验证 4 MB 以下 body 不会被 413 拒绝
- 边界值测试：正好 256 KB、刚好超过 256 KB

**验证命令**:
```bash
cargo test body_limit --no-run
cargo test body_limit
```

---

### M5: 单元测试 - Metrics 401/200
- 测试无 admin token 访问 `/metrics` 返回 401
- 测试带有效 admin token 返回 200
- 测试带无效 token 返回 401
- 沿用已有 admin 鉴权测试的 token fixture

**验证命令**:
```bash
cargo test metrics --no-run
cargo test metrics
```

---

### M6: 质量门禁
- `cargo clippy -- -D warnings` 零警告
- `cargo test` 全部通过
- `cargo build` 成功

**验证命令**:
```bash
cargo clippy -- -D warnings
cargo test
cargo build
```

---

### M7: 文档同步
- 阅读 `docs/TODO.md` 中 SEC-010 和 SEC-007 条目
- 确认完成说明（如需补充 commit 引用、PR 链接）
- 检查 `docs/DOC_CODE_MAP.md` 中是否需要新增条目

**验证命令**:
```bash
grep -n "SEC-010\|SEC-007" docs/TODO.md
```

---

## E2E Checkpoint

- **[E2E-1] 启动服务并 curl 验证**: 完成 M2+M3 后
  ```bash
  cargo run &
  sleep 2
  # 验证 metrics 需要鉴权
  curl -i http://localhost:PORT/metrics   # 期望 401
  curl -i -H "Authorization: Bearer <admin-token>" http://localhost:PORT/metrics  # 期望 200
  # 验证 body limit
  curl -i -X POST -H "Content-Type: application/json" -d @large.json http://localhost:PORT/some-endpoint  # 期望 413
  ```

- **[E2E-2] 完整回归**: M6 通过后，跑全量测试
  ```bash
  cargo test
  cargo clippy -- -D warnings
  ```
