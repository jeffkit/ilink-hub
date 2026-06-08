# Changelog

All notable changes to this project will be documented in this file.

## [0.1.16] — 2026-06-08

### Bridge — 稳定自动注册名

**修复**

- 自动注册默认使用 **`local-<hostname>-<config-stem>`**（如 `local-MacBook-ilink-claude`），不再每次生成随机 `local-<uuid>`，避免 `/list` 堆积失效后端。
- 凭证 JSON 保存 `client_name`；token 失效重注册时复用同一名称。

## [0.1.15] — 2026-06-08

### Hub — 多轮对话 session 连续性

**修复**

- **同一微信用户多轮对话**：微信/iLink 每条消息可能携带新的 `context_token`；Hub 现在按 `peer_user_id`（群聊则按 `group_id`）复用稳定的虚拟 `vctx`，Claude `--resume` 等 backend session ID 可跨消息保留。
- **Hub 重启恢复**：冷启动时从数据库查找该用户已有的 backend session 并预热内存映射。
- **回复来源脚注**：默认仅在 **同时在线的后端 ≥ 2** 时追加 `— 工作区名` 行（不再因历史离线注册项误触发）。

## [0.1.14] — 2026-06-08

### Bridge — Claude Code 可靠性

**修复**

- **YAML `cwd` 支持 `~`**：profile 的 `cwd: ~/projects/foo` 现在会正确展开为用户主目录，避免 spawn 报 `No such file or directory`。
- **`type: claude-code` 自调用**：内置 profile 子进程使用 `current_exe()` 而非依赖 PATH 中的 `ilink-hub-bridge`。
- **Claude 非零 exit 仍解析回复**：当 `claude --output-format json` 因模型错误等返回 exit 1 但 stdout 含 JSON `result` 时，将结果文本转发到微信，而非只显示 `command exited with status 1`。
- **Bridge vtoken 校验与自动重注册**（v0.1.13 起）：Hub 拒绝无效 token 时 bridge 自动删凭证并重新 `/hub/register`。

**说明**

- Profile YAML 的 `env.ILINK_CLAUDE_MODEL` 会注入到 `claude-code` 子进程；用于覆盖 Claude Code 默认模型（例如不可用的第三方模型）。

## [0.1.11] — 2026-06-08

### Bridge — P0 Exec Protocol & Profile SDK

**新功能**

- **P0 协议**：bridge 现在自动将 `ILINK_MESSAGE`、`ILINK_SESSION_ID`、`ILINK_SESSION_NAME`、`ILINK_FROM_USER`、`ILINK_CONTEXT_TOKEN` 注入到每个 profile 进程的环境变量中。自定义脚本和 SDK 无需在 YAML `env:` 段手动映射这些变量。
- **`type: claude-code` 语法糖**：profile 中设置 `type: claude-code` 即可使用内置 Claude Code 处理器，无需配置 `command`、`args`、`cli_session_first_line_prefix`，也不再需要 `ilink-claude-bridge.sh` 包装脚本。
- **`ilink-hub-bridge profile <type>` 子命令**：内置 profile 以独立子命令形式发布，遵守 P0 exec 协议，可在命令行直接测试：`ILINK_MESSAGE="你好" ilink-hub-bridge profile claude-code`。
- **Node.js SDK**（`sdk/node/`）：`@ilink-hub/profile` — 一个 `createProfile(handler)` 调用即可创建跨平台 profile，含 `loadHistory` / `appendHistory` JSONL 对话历史工具。
- **Python SDK**（`sdk/python/`）：`ilink-bridge-profile` — `create_profile(handler)` 同等功能的 Python 版本。
- **[`docs/bridge/profile-spec.md`](docs/bridge/profile-spec.md)**：新增 Bridge Profile P0 协议规范文档，涵盖协议契约、实现方式对比、YAML 配置示例、状态持久化指南。

**变更**

- `run_cli()` 签名新增 `from_user` 和 `context_token` 参数（内部变更，不影响 YAML 配置）。
- 示例 YAML [`docs/bridge/examples/claude-code-session.profiles.yaml`](docs/bridge/examples/claude-code-session.profiles.yaml) 重写为 `type: claude-code` 风格。

### Hub — 多 Session 支持（v0.1.10 继续）

- `/session list / new / use / delete` 命令
- `backend_sessions` 数据库表与 `active_session_name` 字段

---

## [0.1.10] — 2026-06-07

- Hub 内建多 session 管理（`/session` 命令）
- `ilink_hub_ext` 扩展字段（封装 `session_id`、`session_name`、`cli_session_id`）
- `ilink-claude-bridge.sh` 包装脚本（claude code --resume 连续对话）
