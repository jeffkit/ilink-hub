# Bridge Configuration Reference

This document describes the configuration options for `ilink-hub-bridge`.

## Message Placeholder `{{MESSAGE}}`

In `args`, `cwd`, and `env` values, the placeholder `{{MESSAGE}}` is replaced by the incoming user message.

Example configuration:
```yaml
profiles:
  claude:
    command: claude
    args: ["-p", "{{MESSAGE}}", "--continue"]
    stdin: none
```

::: danger Security Warning
Do NOT use `{{MESSAGE}}` as part of a shell `-c` parameter (e.g., `args: ["-c", "echo {{MESSAGE}}"]`), as this can lead to shell command injection vulnerabilities.
If you need to pass the message safely as input to a process, we recommend using the `stdin: message` mode:
```yaml
profiles:
  safe-command:
    command: my-script.sh
    stdin: message
```
:::
