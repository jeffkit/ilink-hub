# 变异测试（Mutation Testing）

ilink-hub 使用 [`cargo-mutants`](https://mutants.rs/) 对核心业务逻辑进行变异测试，
通过在源代码中注入缺陷并验证测试套件是否能检测到它们，衡量测试的**有效性**（而不仅仅是覆盖率）。

## 快速上手

```bash
# 安装工具
cargo install cargo-mutants

# 预览当前配置会扫描哪些文件 / 变异体
cargo mutants --list-files
cargo mutants --list | wc -l

# 单文件快速检查（推荐日常使用）
# 注意：cargo-mutants ≥27 会把 CLI --file 与配置 examine_globs **合并**，
# 单文件扫描请加 --no-config，否则会扫到全部白名单文件。
RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 \
  --file src/hub/vtoken_hash.rs \
  --output mutants-output/vtoken_hash

# 全量扫描（读取 .cargo/mutants.toml，约数小时）
RUST_TEST_THREADS=1 cargo mutants -j 2 --output mutants-output/full
```

> **重要**：必须设置 `RUST_TEST_THREADS=1`。集成测试之间存在共享 in-memory SQLite
> 的竞争条件，并发运行会导致 flaky failures，影响变异测试的基准判断。

## 目标文件范围（.cargo/mutants.toml）

`examine_globs` 是白名单；`exclude_globs` 在 examine **之后**再过滤。
**禁止**把已列入 examine 的目录整树写进 exclude（2026-07-09 已修复该冲突）。

| 类别 | 文件 | 优先级 |
|------|------|--------|
| 安全关键 | `vtoken_hash` / `relay/auth` / `runtime/crypto` | P0 |
| Hub 核心 | `router` / `registry` / `queue` / `commands` / `dispatch` / `pairing` | P0–P1 |
| 纯函数 | `outbound_label` / `messages` / `quote_route` / `ratelimit` | P1 |
| Store | `context` / `clients` / `sessions` / `messages` | P1 |
| Bridge / MCP / Server | paths / dispatcher / config / executor / manager / probe / builtin / mcp/* / sse_ticket / routes | P2 |
| I/O 密集 | `ilink/login` / `relay/client` / `ilink/upstream` | P2（async 路径可 timeout） |

完整列表见 [`.cargo/mutants.toml`](https://github.com/jeffkit/ilink-hub/blob/main/.cargo/mutants.toml)。

## 基准结果

详见 [baseline.md](baseline.md)。

**目标 Mutation Score**：关键路径 ≥ **80%**；全量扫描低于该线时在 CI summary 告警（不做 PR 硬门禁）。

## 术语说明

| 术语 | 含义 |
|------|------|
| **Caught** | 至少一个测试失败 — 变异体被检测到 ✅ |
| **Missed** | 所有测试通过 — 测试套件未能发现该缺陷 ❌ |
| **Unviable** | 变异体导致编译失败（不计入分数） |
| **Timeout** | 测试超时（视为未捕获） |
| **Mutation Score** | `caught / (caught + missed + timeout)` |

## CI 集成

Workflow：[`.github/workflows/mutation-testing.yml`](../../.github/workflows/mutation-testing.yml)

| 触发 | 行为 |
|------|------|
| 每周一 03:00 UTC | 全量扫描 `examine_globs` |
| `workflow_dispatch` | 可指定单文件 + 并行度 |

结果写入 Job Summary，并上传 `mutants.out/` artifact（保留 30 天）。
**不**纳入 PR 必过质量门（全量耗时过长）。

## 持续推进节奏

1. **改关键路径后**：`cargo mutants --no-config -j 2 --file <path>`
2. **每周**：依赖 CI 全量扫描，对照 baseline，处理新增 missed
3. **扩 examine 前**：先补针对性单测 → 单文件扫到 ≥80% → 再写入 `examine_globs`
4. **良性 missed**：写入 `exclude_re` 并在配置注释中说明理由（勿静默忽略）
5. **暂缓项**：需 mock upstream / 可注入时钟的，记入 exec-plan，勿无限扩白名单

## 相关文档

- [baseline.md](baseline.md) — 分阶段基准与历史
- Exec-plan：`docs/exec-plans/active/mutation-test-coverage/`、`mutation-test-coverage-p2/`
