# plan.md — mutation-test-coverage-p2

## 架构设计

纯测试添加，不修改生产代码。工作流程为"先扫描找盲点，再补测试"：

```
1. 运行 cargo mutants → 找出 missed mutants
2. 分析未捕获变异体的根因
3. 在对应文件的 #[cfg(test)] 模块追加测试
4. 重新验证 mutation score 提升
```

## 里程碑

### M1 — commands.rs：命令解析逻辑

**目标**：对 `/list`、`/use`、`/@name`、`/help`、`/status` 等命令解析逻辑建立变异测试基线。

**预期变异体类型**：
- 字符串前缀匹配 (`starts_with`) 的布尔值反转
- 命令分支 OR/AND 逻辑变换
- 返回值替换（`None` ↔ `Some(...)`）

**验证命令**：
```bash
RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 \
  --file src/hub/commands.rs \
  --output mutants-output/phase5-commands
```

**E2E checkpoint**：not-ready（单元测试）
**目标 Mutation Score**：≥ 80%

---

### M2 — pairing.rs：配对流程逻辑

**目标**：对配对码生成、验证、过期等逻辑建立变异测试基线。

**预期变异体类型**：
- 过期时间比较（`>`、`<` 反转）
- 配对码匹配逻辑（`==`、`!=` 反转）
- 返回值替换

**验证命令**：
```bash
RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 \
  --file src/hub/pairing.rs \
  --output mutants-output/phase5-pairing
```

**E2E checkpoint**：not-ready（单元测试）
**目标 Mutation Score**：≥ 80%

---

### M3 — dispatch.rs：消息分发核心（重点）

**目标**：对消息路由决策、优先级逻辑、错误处理分支建立变异测试基线。

**注意**：dispatch.rs 体量最大（1268 行），变异体数量预计 150-200 个，
单次扫描约 90-120 分钟。建议分批扫描或利用 `--exclude-re` 过滤低价值变异。

**验证命令**：
```bash
RUST_TEST_THREADS=1 cargo mutants --no-config -j 2 \
  --file src/hub/dispatch.rs \
  --output mutants-output/phase5-dispatch
```

**E2E checkpoint**：可能需要集成测试覆盖（dispatch 涉及跨组件调用）
**目标 Mutation Score**：≥ 75%（体量大，允许部分暂缓）

---

## 全局验证

```bash
cargo fmt --all -- --check
cargo clippy -- -D warnings
RUST_TEST_THREADS=1 cargo test
```
