# todo-main.rs Implement Log

修复 `main.rs` 模块（CONS-01, CONS-02）的执行日志。范围严格限定在
`prompt.md` 列出的两项一致性条目，不涉及其他模块重构或依赖升级。

---

## M0 — 基线

### Decisions

- M0 不做代码改动，仅按 `plan.md` M0 要求记录修复前的行为快照，作为
  M1 / M2.1 / M2.2 / M2.3 的回滚/对照基线。
- 验证命令按 task prompt 列出的 6 项全部执行（fmt / clippy / test /
  build / desktop-frontend / desktop-tauri），并额外做了
  `cargo build --release` 以便产出 plan.md M0 要求的 4 个 `--help`
  快照。
- 基线观察到的 CONS-01 / CONS-02 现状已记录在
  `reviews/m0/review-request.yaml` 的 `help_snapshots` 和
  `baseline_observations` 字段，下游里程碑将基于这些字段做 diff。
- 工作区当前 HEAD 已是
  `style: green the quality-gate baseline (clippy -D warnings + cargo fmt)`
  ，clippy / fmt 处于已知干净状态；本里程碑确认它们在新分支上仍然干净。

### Problems

- `cargo clippy -- -D warnings` 在初次启动时一次性 cold-compile 大量
  依赖（sqlx、tauri、reqwest 等），约 24s；后续 `-- -D warnings`
  命中缓存立即返回。M0 走的是 cold 路径，故记录的是完整时间。
- 桌面端 `node_modules` 在 worktree 中不存在，task prompt 给定的
  `ln -s` 命令作用一次即可，无需重复。

### Outcome

- fmt: `cargo fmt --check` exit 0，无输出。
- clippy: `cargo clippy -- -D warnings` 0 warning, exit 0。
- test: `cargo test` 147 passed / 0 failed / 1 ignored (doc-test)。
  - 单元测试 121 / 0
  - 集成测试 7 + 9 + 10
  - 其余测试 binary 0
- build: `cargo build` 与 `cargo build --release` 均 exit 0。
- desktop-frontend: `tsc && vite build` 成功，产出 `dist/index.html`
  (15.77 kB) + `dist/assets/index-B_moKtWr.css` (25.91 kB) +
  `dist/assets/index-DUgnO37h.js` (21.71 kB)，85ms 内完成。
- desktop-tauri: `cargo check --manifest-path .../src-tauri/Cargo.toml`
  exit 0；ilink-hub-desktop v0.1.11 + ilink-hub v0.1.20 均干净。
- `--help` 快照四个全部以原文捕获并写入
  `reviews/m0/review-request.yaml` 的 `help_snapshots` 段：
  - `ilink-hub --help`：about 英文（"iLink-compatible multiplexer hub
    for WeChat ClawBot"），4 个子命令全英文描述。
  - `ilink-hub serve --help`：about "Start the hub server"，`--addr`
    默认值 `0.0.0.0:8765`，env `ILINK_HUB_ADDR`。
  - `ilink-hub register --help`：about "Register a backend client with
    the hub (outputs vtoken to use)"，`--hub-url` 默认值
    `http://localhost:8765`，env `ILINK_HUB_URL`。
  - `ilink-hub-bridge --help`：about "Bridge WeChat (via iLink Hub) to a
    local coding CLI (Claude Code, Codex, …)"，`--hub-url` 默认值
    `http://127.0.0.1:8765`，env `WEIXIN_BASE_URL`。
- 基线缺陷（不在 M0 修复，仅记录）确认：
  - CONS-01：四个 `--help` 全部英文。
  - CONS-02：Hub 地址同时存在 `ILINK_HUB_ADDR` / `ILINK_HUB_URL` /
    `WEIXIN_BASE_URL` 三套环境变量；`serve` 默认监听 `0.0.0.0:8765`；
    register 默认 URL 用 `localhost`，bridge 默认 URL 用 `127.0.0.1`，
    三者格式不一致。
- 提交：`chore(exec-plan): M0 baseline snapshot for todo-main.rs`，
  包含 `reviews/m0/review-request.yaml` 与本 `implement.md` 的 M0
  段落。

---

## M1 — CONS-01（待执行）

由 `reviews/m0/review-request.yaml` 的 `next_milestone` 字段驱动。
预计改动文件：
- `src/main.rs`（Cli struct + Commands enum variant 的 about/help 文案）
- `src/bin/ilink-hub-bridge.rs`（Cli struct 的 about/help 文案）

## M2.1 / M2.2 / M2.3 — CONS-02（待执行）

按 `plan.md` 顺序串行执行，每步独立可验证。

