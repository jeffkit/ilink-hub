# Desktop Bridge Profiles

日期：2026-06-09

## 目标

让桌面版 iLink Hub 能自然衔接 `ilink-hub-bridge`：桌面端随 Hub 自动启动一个内置 Bridge，并提供适合小白用户的 Claude Code profile 管理向导。
新增 `ilink-hub-bridge manager` 模式：用户把现有 bridge YAML 放到 profile 目录中，manager 自动为每个 YAML 启动一个独立 bridge workspace。

## 完成标准

- 桌面端可读写 `~/.ilink-hub/ilink-hub-bridge.yaml`，与 CLI 默认配置共用。
- 首次使用时可一键生成 Claude Code profile 配置，用户只需填写项目目录等常用项。
- 桌面端可展示 Bridge 运行状态、配置路径、当前 profile，并支持启动 / 停止 / 重启。
- 保留高级 YAML 查看 / 编辑入口，避免小白表单覆盖高级配置能力。
- `ilink-hub-bridge manager` 可扫描 profile 目录，按文件启动多个真实 bridge 子进程。
- 每个 profile YAML 使用现有格式，不新增 workspace / metadata 配置；workspace 名从文件名派生。
- 每个 profile 使用独立凭证文件，避免多个 workspace 共享同一 vtoken。
- manager 对子进程崩溃具备基础重启能力，并能在配置文件删除后停止对应子进程。
- 补充必要测试与文档，确保现有 bridge CLI 行为不被破坏。

## 非目标

- 不在第一版做任意 command/script/env/routing 的完整表单化编辑。
- 不引入新的配置存储位置；沿用 CLI 默认路径。
- 不改变 Hub 与普通后端注册协议。
- 不在第一版做 profile 热编辑 UI 或新增 YAML schema。
