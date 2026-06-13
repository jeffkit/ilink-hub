# Plan: 修复 main.rs 模块修复（CONS-01, CONS-02）

## 范围
仅修复 `prompt.md` 列出的 CONS-01、CONS-02，不涉及其他模块重构或依赖升级。

---

## 里程碑

### M0. 基线
- 记录修复前的行为快照，便于回滚/对照。
- 验证命令：
  ```bash
  cargo build --release
  ./target/release/ilink-hub --help
  ./target/release/ilink-hub serve --help
  ./target/release/ilink-hub register --help
  ./target/release/ilink-hub-bridge --help
  ```

---

### M1. CONS-01 — CLI 帮助中文化
- 为 `serve` / `register` / `bridge` 三个顶层子命令的 `about`/`help` 文案补中文（或中英双语）。
- 涉及文件：
  - `src/main.rs`（CLI 定义）
  - `src/bin/ilink-hub-bridge.rs`（bridge CLI 定义）
- 验证命令：
  ```bash
  cargo build --release
  ./target/release/ilink-hub --help        # 中文/中英双语
  ./target/release/ilink-hub serve --help  # 中文/中英双语
  ./target/release/ilink-hub register --help
  ./target/release/ilink-hub-bridge --help # 中文/中英双语
  cargo test
  ```

---

### M2. CONS-02 — Hub 地址变量与默认值统一
拆分三个子步，每步独立可验证。

#### M2.1 统一以 `WEIXIN_BASE_URL` 作为「Hub 地址」主入口
- `ILINK_HUB_ADDR`、`ILINK_HUB_URL` 保留为别名兼容（读取时若主变量未设置则回退）。
- 涉及文件：
  - `src/main.rs`（env 读取与 `--addr` / `--hub-url` 默认值）
  - `src/bin/ilink-hub-bridge.rs`（env 读取与默认值）
- 验证命令：
  ```bash
  WEIXIN_BASE_URL=http://127.0.0.1:9000 ./target/release/ilink-hub register --help
  ILINK_HUB_ADDR=http://127.0.0.1:9000 ./target/release/ilink-hub register --help   # 别名仍生效
  ILINK_HUB_URL=http://127.0.0.1:9000 ./target/release/ilink-hub register --help    # 别名仍生效
  ```

#### M2.2 文档示例统一为 `127.0.0.1`
- 将 `docs/guide/getting-started.md`、`docs/bridge/quick-try.md` 中涉及的 `0.0.0.0:8765` / `localhost:8765` 示例统一为 `127.0.0.1:8765`。
- 在「部署/安全」章节显式说明：需要对外暴露时显式传 `0.0.0.0`。
- 涉及文件：
  - `docs/guide/getting-started.md`
  - `docs/bridge/quick-try.md`
- 验证命令：
  ```bash
  grep -RIn -E 'localhost|0\.0\.0\.0' docs/guide/getting-started.md docs/bridge/quick-try.md
  # 仅「部署/安全」章节出现 0.0.0.0；其余示例均为 127.0.0.1
  ```

#### M2.3 调整 `serve` 默认监听为 `127.0.0.1:8765`
- 默认监听地址由 `0.0.0.0:8765` 改为 `127.0.0.1:8765`。
- 需要 LAN/容器暴露时由用户显式传 `0.0.0.0`。
- 在 `CHANGELOG` 标注行为变更。
- 涉及文件：
  - `src/main.rs`
  - `CHANGELOG`（新增条目）
- 验证命令：
  ```bash
  ./target/release/ilink-hub serve &
  sleep 1
  ss -tlnp | grep ilink-hub   # 仅 127.0.0.1:8765 监听，不应有 0.0.0.0:8765
  curl -sf http://127.0.0.1:8765/health || echo "health endpoint missing"
  ./target/release/ilink-hub serve --addr 0.0.0.0:8765 &
  sleep 1
  ss -tlnp | grep ilink-hub   # 此时应监听 0.0.0.0:8765
  pkill -f ilink-hub
  ```

---

### M3. 质量门
- 完成标准全部勾选。
- 验证命令：
  ```bash
  cargo fmt --all -- --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```

---

## E2E Checkpoints

| 标记 | 位置 | 验证动作 |
| --- | --- | --- |
| **E2E-1** | M1 完成后 | 用户视角：分别运行 `ilink-hub --help`、`serve --help`、`register --help`、`ilink-hub-bridge --help`，确认中文/中英双语 help 文本展示正常。 |
| **E2E-2** | M2.1 完成后 | 通过设置 `WEIXIN_BASE_URL` / `ILINK_HUB_ADDR` / `ILINK_HUB_URL` 三种 env，观察 register / bridge 行为一致（指向同一 Hub）。 |
| **E2E-3** | M2.3 完成后 | 启动 `ilink-hub serve`，从同机 `curl 127.0.0.1:8765` 可达；从同网段另一台主机 `curl <host-ip>:8765` **不可达**，证明默认不再对局域网开放。显式 `--addr 0.0.0.0:8765` 时再次验证可达。 |

---

## 完成标准映射

- [ ] CONS-01 → M1 验证通过
- [ ] CONS-02 → M2.1 / M2.2 / M2.3 全部验证通过
- [ ] `cargo clippy` 无新 warning → M3
- [ ] `cargo test` 全绿 → M3