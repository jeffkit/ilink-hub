# Bridge Configuration Reference

This document describes the configuration options for `ilink-hub-bridge`.

## AgentProc 0.4 NDJSON Protocol

Bridge writes one NDJSON turn object to the profile process stdin and reads NDJSON events (`partial` / `result` / `error` / `permission_request`) from stdout. Env vars are for secrets/config only. See [profile protocol](knowledge/bridges/profile-protocol.md).

## YAML hub form（一文件一 profile）

```yaml
description: optional
script: ./handler.py          # optional; agentproc.command wins
agentproc:
  executor: claude-code       # or command/args for custom spawn
  cwd: /path/to/project
  env:
    ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
    CLAUDE_MODEL: sonnet
  streaming: true
  permission: false
  send_error_reply: true
  timeout_secs: 1800
  max_reply_chars: 8000
```

| 字段 | 默认 | 说明 |
|------|------|------|
| `agentproc.executor` | 无 | 内置 executor 名 |
| `agentproc.command` / `args` | 空 | 自定义命令 |
| `agentproc.cwd` | 进程 cwd | 工作目录 |
| `agentproc.env` | `{}` | `${VAR}` 从 bridge 进程 env 展开 |
| `agentproc.env_allowlist` | 无 | 限制可展开变量 |
| `agentproc.timeout_secs` | `1800` | 超时秒数 |
| `agentproc.kill_grace_secs` | `5` | 优雅退出宽限 |
| `agentproc.max_reply_chars` | `8000` | 截断上限 |
| `agentproc.truncation_suffix` | `…(输出已截断)` | 截断后缀 |
| `agentproc.streaming` | `true` | 是否转发 `partial` |
| `agentproc.permission` | `false` | permission 通道；请求恒 allow |
| `agentproc.send_error_reply` | `true` | 错误是否回用户 |
| `agentproc.include_stderr_in_reply` | `false` | 是否拼 stderr |
| `description` / `script` | — | 文件级 sibling |

**已移除**：`profiles` / `routing` / `type` / `skip_bot_messages` / `require_text` / 顶层 `send_error_reply` / `permission_default` / `permission_ask_timeout_secs`。入站过滤（跳过 bot、要求文本）为固定逻辑。

## Message Placeholder `{{MESSAGE}}`

自定义 CLI 可在 `args` / `cwd` / `env` 值中使用 `{{MESSAGE}}` / `{{SESSION_ID}}` / `{{SESSION_NAME}}` / `{{PROFILE_DIR}}`。

```yaml
description: echo with placeholder
agentproc:
  command: echo
  args: ["{{MESSAGE}}"]
```

::: danger Security Warning
Do NOT use `{{MESSAGE}}` as part of a shell `-c` parameter. The bridge rejects shell + `-c` + `{{MESSAGE}}` at load time.
:::
