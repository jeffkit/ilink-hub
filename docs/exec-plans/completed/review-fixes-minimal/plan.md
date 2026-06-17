# Plan: review-fixes-minimal

## 里程碑

### M1: 安全文档与 Docker 示例完善 (B-1)
- [ ] `README.md` 中 `ILINK_ADMIN_TOKEN` 标注为必填，`ILINK_ADMIN_INSECURE_NO_AUTH=true` 添加安全警告
- [ ] `deploy/` 下 Docker Compose 示例中 `ILINK_ADMIN_TOKEN` 标注强烈建议设置

**验证**:
```bash
grep -n "ILINK_ADMIN_TOKEN\|ILINK_ADMIN_INSECURE_NO_AUTH" README.md deploy/*.yml deploy/*.yaml 2>/dev/null
```

### M2: AUTH_ERROR_KEYWORDS 常量提取 (NEW-1)
- [ ] `src/bridge/mod.rs` 新增 `const AUTH_ERROR_KEYWORDS: &[&str]`
- [ ] `handle_one_message` 和 `dry_run_profile` 均使用该常量

**验证**:
```bash
grep -n "AUTH_ERROR_KEYWORDS" src/bridge/mod.rs
```

### M3: bridge 超时行为文档化 (NEW-2)
- [ ] `BridgeProfile::timeout_secs` 字段注释说明超时叠加行为（最坏情况 `timeout_secs + 10s`）

**验证**:
```bash
grep -A2 "timeout_secs" src/bridge/mod.rs
```

### M4: 最终质量门
- [ ] `cargo clippy -- -D warnings` 零警告
- [ ] `cargo test` 全部通过

**验证**:
```bash
cargo clippy -- -D warnings && cargo test
```

## E2E Checkpoint

| 标记 | 说明 |
|------|------|
| `E2E_READY` | M1-M3 全部完成，可执行最终质量门 M4 |
| `E2E_PASS` | M4 通过，分支可合并 |
