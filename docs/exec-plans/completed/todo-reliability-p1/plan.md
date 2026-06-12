# Plan: P1 可靠性修复 (DB-01, E-03)

## 里程碑

### M1 — DB-01: SQLite AnyPool 多连接并发问题修复
- 修改 `src/store/mod.rs:34-41`,为文件型 SQLite 路径 pin `max_connections(1)` 并启用 `busy_timeout`。
- 验证命令:
  - `cargo build -p ilink-hub` 通过
  - `cargo test -p ilink-hub store::` 全绿
  - 手动复现: `persist_context_tokens_batch` 与 `get_active_session_name` 并发不返回 `SQLITE_BUSY`

### M2 — E-03: relay 客户端 shutdown 信号接入
- 修改 `src/relay/client.rs:18-29`,为 `spawn_relay_client` 注入 `watch::Receiver<bool>`,loop 内 `tokio::select!` 包裹 sleep / run_session,命中 shutdown 时 `return`。
- 验证命令:
  - `cargo build -p ilink-hub` 通过
  - `cargo test -p ilink-hub relay::` 全绿
  - 关闭 hub 时 relay 服务端不再看到异常断开 (日志检查 graceful close)

### M3 — 质量门禁收尾
- 验证命令:
  - `cargo clippy --all-targets -- -D warnings` 无新 warning
  - `cargo test` 全绿

## E2E Checkpoints

- [E2E-1] **M1 完成后**: 启动 hub,在并发负载 (多 session + 多次 token 持久化) 下无 `SQLITE_BUSY` 错误日志。
  - 命令: `RUST_LOG=sqlx=warn cargo run -p ilink-hub -- --config config/dev.toml 2>&1 | tee /tmp/hub-m1.log`
  - 通过标准: grep `SQLITE_BUSY` 结果为空;30s 内 hub 健康检查返回 200。

- [E2E-2] **M2 完成后**: 启动 hub + relay,发送 SIGTERM,relay 端日志出现 `graceful shutdown` / `connection closed by peer (clean)`,无 `connection reset` / `unexpected EOF`。
  - 命令:
    1. `cargo run -p ilink-relay &`  (或对照既有 e2e 脚本)
    2. `cargo run -p ilink-hub &`
    3. `kill -TERM <hub_pid>` ; sleep 2
    4. 抓取 relay 端日志,断言无异常断开关键字。
  - 通过标准: hub 进程在 5s 内退出;relay 端连接清理日志正常。

- [E2E-3] **M3 完成后**: 全量 CI 等价命令本地复跑通过。
  - 命令: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all`
  - 通过标准: 三条命令 exit code 全部为 0。
