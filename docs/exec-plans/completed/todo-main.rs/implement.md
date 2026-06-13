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

## M1 — CONS-01 — CLI 帮助中文化

### Decisions
- 将 `ilink-hub` 主命令、`serve` 子命令、`register` 子命令以及 `ilink-hub-bridge` 主命令的 `about`/`help` 文案更新为中英双语。
- 采用双语格式以最大程度保持和原有系统的一致性，防止第三方系统强依赖原英文部分。
- 调整后通过 `cargo fmt` 自动格式化对超长属性进行换行折叠。

### Problems
- 初始提交时 `cargo fmt --check` 因属性超长未格式化报错，已通过 `cargo fmt` 自动排版解决。

### Outcome
- fmt: `cargo fmt --check` 成功通过。
- clippy: `cargo clippy -- -D warnings` 通过，无任何 warnings。
- test: `cargo test` 全绿通过（149 passed）。
- build: `cargo build` 及 `cargo build --release` 编译成功。
- desktop-frontend: Vite 构建和 TypeScript 检查通过，成功输出 dist。
- desktop-tauri: Tauri `src-tauri` 检查通过。
- 命令验证：
  - `ilink-hub --help` 显示：`微信 ClawBot 的 iLink 兼容多路复用 Hub / iLink-compatible multiplexer hub for WeChat ClawBot`
  - `serve --help` 显示：`启动 Hub 服务器 / Start the hub server`
  - `register --help` 显示：`向 Hub 注册客户端（输出可用的 vtoken） / Register a backend client with the hub (outputs vtoken to use)`
  - `ilink-hub-bridge --help` 显示：`将微信（通过 iLink Hub）桥接到本地编码 CLI (Claude Code, Codex, …) / Bridge WeChat (via iLink Hub) to a local coding CLI (Claude Code, Codex, …)`
- 在 `reviews/m1/review-request.yaml` 记录了所有执行结果与快照。

## M2.1 — 统一以 WEIXIN_BASE_URL 作为「Hub 地址」主入口

### Decisions
- 将 `main.rs` 的 `--addr`、`--hub-url` 参数以及 `ilink-hub-bridge.rs` 的 `hub_url` 参数的主环境变量统一为 `WEIXIN_BASE_URL`。
- 在 `main.rs` 和 `ilink-hub-bridge.rs` 中实现 `get_hub_url_default()` 和 `get_addr_default()` 辅助函数，使 CLI 在 `WEIXIN_BASE_URL` 未设置时能依次降级回退读取别名环境变量 `ILINK_HUB_URL` 和 `ILINK_HUB_ADDR`，保证前向兼容性。
- 对 `serve` 命令的 `addr` 参数进行运行时拦截，若读取到的值是 URL 形式（例如 `http://127.0.0.1:9000`），则提取其主机名与端口部分（`127.0.0.1:9000`），从而避免由于传递了 URL 协议头而导致 `TcpListener::bind` 报错。
- 在 `tests/breaking_changes.rs` 增加 `test_cli_hub_url_env_fallback` 和 `test_bridge_hub_url_env_fallback` 两个集成测试，以全面覆盖不同环境变量优先级和回退逻辑。

### Problems
- 无。逻辑与单元/集成测试运行顺利，未遇到阻碍。

### Outcome
- fmt: `cargo fmt --check` 成功通过。
- clippy: `cargo clippy -- -D warnings` 通过，无任何 warnings。
- test: `cargo test` 全绿通过（151 passed）。
- build: `cargo build` 及 `cargo build --release` 编译成功。
- desktop-frontend: Vite 构建和 TypeScript 检查通过，成功输出 dist。
- desktop-tauri: Tauri `src-tauri` 检查通过。
- 验证命令结果：
  - `WEIXIN_BASE_URL=http://127.0.0.1:9000 ./target/release/ilink-hub register --help` 成功显示 `[env: WEIXIN_BASE_URL=http://127.0.0.1:9000] [default: http://127.0.0.1:9000]`。
  - `ILINK_HUB_ADDR=http://127.0.0.1:9000 ./target/release/ilink-hub register --help` 成功兼容回退显示 `[default: http://127.0.0.1:9000]`。
  - `ILINK_HUB_URL=http://127.0.0.1:9000 ./target/release/ilink-hub register --help` 成功兼容回退显示 `[default: http://127.0.0.1:9000]`。
  - `WEIXIN_BASE_URL=http://127.0.0.1:9000 ./target/release/ilink-hub serve --help` 成功提取出 `[default: 127.0.0.1:9000]`。
- 在 `reviews/m2.1/review-request.yaml` 记录了所有执行结果与快照。

---

## M2.2 — 文档示例统一为 127.0.0.1

### Decisions
- 将 `docs/guide/getting-started.md` 里所有 CLI 示例以及对应的环境变量配置/各语言 SDK 配置中的 `0.0.0.0:8765` / `localhost:8765` 统一修改为本机环回地址 `127.0.0.1:8765`，保持风格一致性。
- `docs/bridge/quick-try.md` 中不含有 `localhost` / `0.0.0.0` 示例，已全为 `127.0.0.1`，无需更改。
- 在 `docs/deployment/security.md` 中的「## 3. 网络隔离」段落追加关于 Hub 默认仅监听本机环回地址以确保安全性，若需对外暴露（如在 Docker 容器、虚拟机或局域网中运行）则必须显式传 `--addr 0.0.0.0:8765` 参数的说明，引导用户安全配置。

### Problems
- 无。

### Outcome
- fmt: `cargo fmt --check` 成功通过。
- clippy: `cargo clippy -- -D warnings` 通过，无任何 warnings。
- test: `cargo test` 全绿通过（154 passed）。
- build: `cargo build` 编译成功。
- desktop-frontend: Vite 构建和 TypeScript 检查通过，成功输出 dist。
- desktop-tauri: Tauri `src-tauri` 检查通过。
- 验证命令结果：
  - `grep -RIn -E 'localhost|0\.0\.0\.0' docs/guide/getting-started.md docs/bridge/quick-try.md` 返回 exit code 1（无匹配项），表明除了被修改的说明外，全部示例均已统一为 `127.0.0.1`。
- 在 `reviews/m2.2/review-request.yaml` 记录了所有执行结果与说明。

---

## M2.3 — 调整 serve 默认监听为 127.0.0.1:8765

### Decisions
- 将 `serve` 默认监听地址从 `0.0.0.0:8765` 调整为 `127.0.0.1:8765`（该安全性变更在 M0 修复中已预先引入，并在 M2.1 中合并了 `WEIXIN_BASE_URL` 解析逻辑）。
- 在 `CHANGELOG.md` 的 `[Unreleased]` 中增加了关于监听地址调整的「安全变更」条目。
- 后续需要局域网或容器内暴露时，由用户显式传递 `--addr 0.0.0.0:8765` 参数。
- 本地在 macOS 上运行后台服务，并通过 `lsof -i :8765` 验证其在默认情况下仅监听 `localhost:8765`，而显式配置 `--addr 0.0.0.0:8765` 时能够正确监听在所有接口。

### Problems
- 验证命令中的 `ss -tlnp` 在 macOS 上不可用（报 `command not found: ss`），因此改为在 macOS 上等效的 `lsof -i :8765` 进行网络端口监听验证，效果完全一致。

### Outcome
- fmt: `cargo fmt --check` 成功通过。
- clippy: `cargo clippy -- -D warnings` 通过，无任何 warnings。
- test: `cargo test` 全绿通过（154 passed）。
- build: `cargo build` 编译成功。
- desktop-frontend: Vite 构建和 TypeScript 检查通过，成功输出 dist。
- desktop-tauri: Tauri `src-tauri` 检查通过。
- 验证命令结果：
  - 默认启动：`lsof -i :8765` 输出 `TCP localhost:ultraseek-http (LISTEN)`。
  - `/health` 请求：`curl -sf http://127.0.0.1:8765/health` 成功返回 `ok`。
  - `--addr 0.0.0.0:8765` 启动：`lsof -i :8765` 输出 `TCP *:ultraseek-http (LISTEN)`。
- 在 `reviews/m2.3/review-request.yaml` 中提交了审核请求。

---

## M3 — 质量门

### Decisions
- 作为一个没有功能性代码变更的纯质量把关里程碑，重点在于对整个 Hub 和桌面端的前后端代码进行一致性、格式以及警告层面的全面核验。
- 执行了完整的静态分析和构建任务，包括：
  1. Rust 格式化检查 (`cargo fmt --check`)
  2. Rust 静态分析 Clippy 无警告 (`cargo clippy -- -D warnings` / `--all-targets`)
  3. 全部 154 个单元/集成测试通过 (`cargo test`)
  4. Rust 主工程编译通过 (`cargo build`)
  5. 桌面端前端构建通过 (`npm run build`)
  6. 桌面端 Tauri Rust 部分检查通过 (`cargo check --manifest-path ...`)

### Problems
- 无。全部六项质量把关命令在此前解决完 M2.3 变更后，执行均无任何报错或警告，直接全绿通过。

### Outcome
- fmt: `cargo fmt --check` 成功通过。
- clippy: `cargo clippy -- -D warnings` 通过，无任何 warnings。
- test: `cargo test` 全绿通过（154 passed）。
- build: `cargo build` 编译成功。
- desktop-frontend: Vite 构建和 TypeScript 检查通过，成功输出 dist。
- desktop-tauri: Tauri `src-tauri` 检查通过。
- 在 `reviews/m3/review-request.yaml` 中提交了审核请求。

