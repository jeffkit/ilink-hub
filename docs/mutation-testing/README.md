# 变异测试（Mutation Testing）

ilink-hub 使用 [`cargo-mutants`](https://mutants.rs/) 对核心业务逻辑进行变异测试，
通过在源代码中注入缺陷并验证测试套件是否能检测到它们，衡量测试的**有效性**（而不仅仅是覆盖率）。

## 快速上手

```bash
# 安装工具
cargo install cargo-mutants

# 针对 Phase 1 核心模块快速检查（约 48 分钟）
RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 \
  --file src/hub/vtoken_hash.rs \
  --file src/hub/outbound_label.rs \
  --file src/relay/ratelimit.rs \
  --output mutants-output/phase1

# 针对所有配置的目标文件运行（含 router、registry、queue 等）
RUST_TEST_THREADS=1 cargo mutants -j 2 --output mutants-output/full
```

> **重要**：必须设置 `RUST_TEST_THREADS=1`。集成测试之间存在共享 in-memory SQLite
> 的竞争条件，并发运行会导致 flaky failures，影响变异测试的基准判断。

## 目标文件范围（.cargo/mutants.toml）

| 文件 | 说明 | 优先级 |
|------|------|--------|
| `src/hub/vtoken_hash.rs` | 安全关键：vtoken SHA-256 哈希 | P0 |
| `src/relay/auth.rs` | 安全关键：Ed25519 配对注册签名验证 | P0 |
| `src/hub/outbound_label.rs` | 出站消息格式化（纯函数，分支密集） | P1 |
| `src/relay/ratelimit.rs` | 固定窗口限流逻辑 | P1 |
| `src/hub/router.rs` | Hub 路由表 | P1 |
| `src/hub/registry.rs` | 客户端注册表 | P1 |
| `src/hub/queue.rs` | 消息队列实现 | P1 |
| `src/hub/health.rs` | 健康检查逻辑 | P2 |

## 基准结果

详见 [baseline.md](baseline.md)。

## 术语说明

| 术语 | 含义 |
|------|------|
| **Caught** | 至少一个测试失败 — 变异体被检测到 ✅ |
| **Missed** | 所有测试通过 — 测试套件未能发现该缺陷 ❌ |
| **Unviable** | 变异体导致编译失败（不计入分数） |
| **Timeout** | 测试超时（视为未捕获） |
| **Mutation Score** | `caught / (caught + missed + timeout)` |

## CI 集成（规划）

目前变异测试仅在本地按需运行，不纳入 PR CI（耗时 ~1h）。
未来计划：每周定时跑完整扫描，结果上传为 CI artifact，并在
mutation score 低于基准线 80% 时告警。
