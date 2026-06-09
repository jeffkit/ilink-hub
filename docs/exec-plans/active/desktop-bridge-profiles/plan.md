# Desktop Bridge Profiles Plan

## 范围

单仓改动，涉及桌面端 Tauri shell、前端 UI、bridge 配置辅助能力与文档。

## 设计

1. 在桌面端 Rust 层新增 `BridgeController`，管理一个内置 bridge 任务。
2. Bridge 使用同一进程内的 `BridgeApp::load` + `run_bridge`，不要求用户额外安装 `ilink-hub-bridge` 二进制来启动主 bridge。
3. 首次没有配置时，桌面端生成 Claude Code profile YAML 到 `~/.ilink-hub/ilink-hub-bridge.yaml`。
4. `ilink-hub-bridge manager` 作为上层进程管理器：
   - 扫描 `~/.ilink-hub-bridge/profiles/*.yaml`；
   - 每个 YAML 使用现有 bridge 配置格式；
   - 按文件名派生 workspace/register name；
   - 为每个 YAML 使用独立 cred file；
   - 启动真实 `ilink-hub-bridge --config ...` 子进程并在异常退出后退避重启。
5. UI 新增「Bridge」页，提供：
   - 当前运行状态；
   - 一键创建 / 更新 Claude Code profile；
   - 项目目录、超时、最大回复长度、模型；
   - 高级 YAML 查看 / 保存；
   - 启动 / 停止 / 重启。

## 验证命令

- `cargo test`
- `npm --prefix desktop/ilink-hub-desktop run build`
- 如环境允许：`cargo test -p ilink-hub --lib bridge`
- `cargo test manager`

## 风险

- 桌面端自动启动 bridge 时 Hub 可能尚未完成监听，需要监听地址就绪后再启动。
- 高级 YAML 和小白向导可能互相覆盖，需要只在用户明确保存向导时写入。
- 现有工作区已有 bridge 相关改动，修改时必须保留当前内容。
- manager 是进程管理器，需要避免崩溃后忙重启、共享凭证、以及被继承的 `WEIXIN_TOKEN` / `ILINKHUB_BRIDGE_CREDS` 破坏 workspace 隔离。
