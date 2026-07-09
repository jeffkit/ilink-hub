# security-hardening-abc — Prompt

> 创建日期：2026-07-09 | 状态：active | 模式：单仓
> 分支：`fix/security-hardening-abc`
> Worktree：`.worktrees/fix/security-hardening-abc/`

## 目标

修复架构审计确认的 Critical/High 安全与可靠性问题，并按用户确认推进 god 模块拆分与文档债清理，使 Hub 的安全加固真正生效、配对凭证不可被反复窃取、危险 profile 配置无法静默放行。

## 用户确认边界

- **做**：A（CORS 接线 + vtoken 单次领取）、B（shell 硬拒绝、DATABASE_URL 脱敏、桌面强制 loopback）、C=3（真拆多个 god 模块 + 归档过期 plan + 队列产品限制说明）
- **不做**：修改 builtin bridge 的 `--dangerously-skip-permissions` / `--yolo` 等 dangerous flag（当前无用户授权渠道）

## 完成标准

- [ ] `build_router` 使用 `build_cors_layer()`；设置 `ILINK_CORS_ORIGINS` 时非白名单 Origin 不被放行（router 级测试）
- [ ] `get_qrcode_status` 在 confirmed 后仅首次返回明文 `bot_token`，再次轮询不再返回该 token
- [ ] profile 使用 shell + `-c` + `{{MESSAGE}}` 时加载失败（硬拒绝），有回归测试
- [ ] 启动日志不再打印含密码的完整 `DATABASE_URL`
- [ ] 桌面端 `ILINK_HUB_ADDR` 非 loopback 时拒绝启动/解析失败
- [ ] 至少拆分 2 个 god 模块（优先 `dispatcher` / `desktop lib` / `routes`），单文件行数显著下降且测试通过
- [ ] 文档写明队列仅 memory、重启丢消息；过期 active exec-plan 归档或修正 status
- [ ] `cargo fmt --check` / `clippy -D warnings` / `cargo test` / `cargo build` 通过

## 非目标

- 不关闭或默认关闭 builtin dangerous CLI flags
- 不实现 Redis/持久化队列 backend（仅文档化产品限制）
- 不改公网 relay 默认开关（可在文档中提示生产关闭）
