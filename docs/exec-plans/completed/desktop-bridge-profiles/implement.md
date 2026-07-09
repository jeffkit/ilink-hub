# Desktop Bridge Profiles Implement Log

## M1 - Desktop Bridge Integration

### Decisions

- 采用桌面端内置 bridge 任务，不要求用户手动运行 CLI。
- 配置路径沿用 `~/.ilink-hub/ilink-hub-bridge.yaml`，和 CLI 共用。
- 第一版小白向导聚焦 Claude Code profile，保留高级 YAML 编辑入口。

### Problems

- Tauri 的 async task handle 不提供 `is_finished()`，改为用自有运行状态判断是否处于 `starting` / `running`。
- `ApplyPatch` 对 `docs/bridge/quick-try.md` 的 Markdown 容器上下文匹配失败，最终只同步了快速开始入口文档。

### Outcome

- 桌面端新增 Bridge 配置读写、Claude Code profile 向导保存、高级 YAML 保存，以及启动 / 停止 / 重启命令。
- 桌面端新增「Bridge」页，并在 Hub 就绪且配置有效时自动启动内置 Bridge。
- `BridgeApp` 暴露 profile 查询与默认 profile 名，供桌面端展示配置摘要。
- 补充桌面端向导 YAML 生成单测。

## M2 - Bridge Manager

### Decisions

- 新增 `ilink-hub-bridge manager`，作为纯进程管理器运行，不直接处理消息。
- 每个 `*.yaml` / `*.yml` profile 文件保持现有 bridge YAML 格式，不新增 metadata。
- workspace / register name 从文件名派生，凭证写到独立的 `<profile>.json`，避免共享 vtoken。
- manager 子进程显式移除 `WEIXIN_TOKEN` / `ILINKHUB_BRIDGE_CREDS` / `ILINKHUB_BRIDGE_REGISTER_NAME`，防止继承环境破坏 workspace 隔离。
- manager 暴露库级 `spawn_bridge_manager` + `BridgeManagerHandle`，为桌面端后续内嵌管理保留停止和状态查询能力。
- 子进程反复退出时使用指数退避，默认 5s 起步、60s 封顶，避免错误配置或 Hub 故障时忙重启。

### Problems

- `--token` / `--pair` 等全局 CLI 参数在 manager 语义下不适合复用到多个 profile；当前选择记录 warning 并忽略，由每个子 bridge 自动注册。
- 审查发现不同文件名可能清洗成同一 profile id，已改为对冲突项追加稳定短 hash 后缀，并补充单测。
- 复查发现 handle 被直接 drop 可能导致 watch receiver 空转、退避计数长期不归零、状态快照不便于桌面端序列化；已修复并补充测试。

### Outcome

- 新增 `bridge::manager` 模块，支持目录扫描、YAML 校验、文件变更重启、删除停止、子进程异常退出后退避重启。
- 新增 manager 默认路径：`~/.ilink-hub-bridge/profiles` 与 `~/.ilink-hub-bridge/credentials`。
- 补充 manager 单测覆盖 profile id 派生、冲突消解、目录发现、无效 YAML 跳过、独立子进程参数、指数退避和 handle 停止。

## M3 - Desktop Profile Manager

### Decisions

- 桌面端 Bridge 页从单配置文件向导升级为 profiles 目录管理中心。
- 不迁移旧 `~/.ilink-hub/ilink-hub-bridge.yaml`，因为该能力尚未对外发布。
- 桌面端启动一个内嵌 manager task，而不是直接运行单个 `BridgeApp::run_bridge`。
- 每个 profile 的 workspace 名等于 YAML 文件名，修改 workspace 名等价于重命名文件。
- 快捷模板第一版覆盖 Claude Code、Cursor Agent、Codex、Gemini；复杂配置保留 Custom YAML 编辑。

### Problems

- Gemini CLI 没有仓库内现成示例，模板使用保守的 `gemini -p "{{MESSAGE}}"`，UI 保留命令与 args 可编辑能力。
- 旧 `bridge_config` / `bridge_save_yaml` 命令仍保留以降低改动风险，但新 UI 已切到 `bridge_profiles` / `bridge_save_profile` / `bridge_delete_profile`。
- 审查发现退出兜底、profile 覆盖保护、`.yml` 文件定位、`/new` 复选框和 README 默认值问题，已全部修复。

### Outcome

- 桌面端新增 profile 列表、模板表单、高级 YAML 编辑和删除确认。
- BridgeController 改为管理内嵌 manager handle，启动/停止/重启作用于 manager。
- 状态面板显示 profiles 目录，profile 列表展示运行状态、pid、uptime、重启次数和错误摘要。
- 补充快捷模板生成测试，覆盖 Cursor/Codex/Gemini。
- 保存 profile 时会阻止覆盖其它已存在 workspace；删除/编辑支持 `.yaml` 与 `.yml` 两种后缀。
