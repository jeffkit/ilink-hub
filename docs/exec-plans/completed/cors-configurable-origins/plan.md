# Plan: cors-configurable-origins

## M1: 核心实现

修改 `src/server/mod.rs`，读取 `ILINK_CORS_ORIGINS` 环境变量，用 `CorsLayer::new().allow_origin(list)` 替代硬编码的 `CorsLayer::permissive()`。

**验证命令：**
```bash
cargo build
cargo test --lib server
```

**E2E checkpoint <M1>**：`ILINK_CORS_ORIGINS=https://a.com` 启动服务后，非白名单 origin 的请求被 CORS 拒绝。

---

## M2: 边界处理

- 未设置时保持 permissive + WARN 日志
- 非法 origin 格式（不含 scheme）启动报错

**验证命令：**
```bash
# permissive fallback 日志
ILINK_CORS_ORIGINS="" cargo run 2>&1 | grep -i "WARN.*cors"

# 非法格式报错
ILINK_CORS_ORIGINS="bad-origin" cargo run; echo "exit: $?"
```

**E2E checkpoint <M2>**：未设置时行为与 permissive 一致；非法格式直接报错退出。

---

## M3: 集成测试

编写集成测试覆盖 CORS 行为。

**验证命令：**
```bash
cargo test --test cors_tests
```

**E2E checkpoint <M3>**：测试覆盖 permissive 回退、白名单允许/拒绝、非法格式报错。

---

## M4: 文档与质量门禁

**验证命令：**
```bash
cargo test
cargo clippy -- -D warnings
grep -r "ILINK_CORS_ORIGINS" README.md docs/
```
