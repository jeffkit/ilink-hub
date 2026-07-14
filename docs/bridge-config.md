# Bridge Configuration Reference

This document describes the configuration options for `ilink-hub-bridge`.

## AgentProc 0.3 NDJSON Protocol

Bridge 与 profile 进程之间采用 AgentProc 0.3 NDJSON 协议：bridge 向 profile stdin 写一行 turn 对象，profile 在 stdout 逐行输出 NDJSON 事件（`partial` / `text` / `session` / `error` / `permission_request`）。环境变量仅用于密钥与配置。详见 [AgentProc 0.3 协议规范](/bridge/profile-spec)。

## Message Placeholder `{{MESSAGE}}`

自定义 CLI profile 若需要把用户消息作为命令行参数传入，可在 `args` / `cwd` / `env` 值中使用占位符 `{{MESSAGE}}`（以及 `{{SESSION_ID}}` / `{{SESSION_NAME}}` / `{{PROFILE_DIR}}`），bridge 会从 turn 对象中取值替换。

Example configuration:
```yaml
profiles:
  claude:
    command: claude
    args: ["-p", "{{MESSAGE}}", "--continue"]
```

::: danger Security Warning
Do NOT use `{{MESSAGE}}` as part of a shell `-c` parameter (e.g., `args: ["-c", "echo {{MESSAGE}}"]`), as this can lead to shell command injection vulnerabilities. The bridge rejects the dangerous combination of a shell command with `-c` and `{{MESSAGE}}` in args/env at config load time.
:::

## 0.3 新增字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `env_allowlist` | 无 | 限制 `env` 变量展开的允许名单；未列出的未知变量展开为空（POSIX 语义） |
| `kill_grace_secs` | 见 `default_kill_grace_secs` | 超时后优雅退出宽限秒数 |
| `permission` | `false` | 开启 permission 通道（stdin 保持开启以接收 `permission_response`） |
| `permission_default` | `allow` | permission 默认策略：`allow` / `deny` / `deny_logged` / `ask`（`ask` 暂停 turn 走微信交互审批） |
| `permission_ask_timeout_secs` | `600` | `ask` 策略等待用户回复秒数；超时自动 deny 并提示用户 |
| `truncation_suffix` | `…(已截断)` | 超长回复截断后缀 |
| `streaming` | `true` | bridge 侧 hint：是否转发 `partial` 事件 |

> 0.2 的 `stdin`（`StdinMode`）与 `cli_session_first_line_prefix` 字段已移除；消息统一经 stdin NDJSON turn 传递。
