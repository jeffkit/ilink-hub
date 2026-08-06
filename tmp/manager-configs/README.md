# tcloud_gz · im-agentproc manager 模式运行配置

> 镜像本地 Mac 的 `com.ilink-hub.bridge-manager` plist：`im-agentproc manager` 每 5s 扫描
> `~/.ilink-hub-bridge/profiles/`，一文件 = 一个独立 Hub backend。
> 部署于 2026-07-22，systemd 服务 `im-agentproc.service`（`active`）。

## 目录结构（服务器实际路径）

```
/etc/systemd/system/im-agentproc.service          # systemd 单元（manager 模式）
/home/ubuntu/.ilink-hub/bridge.env                # ILINK_ADMIN_TOKEN 等（EnvironmentFile，原样保留）
/home/ubuntu/.ilink-hub/claude.env                # ANTHROPIC_* 等（EnvironmentFile，原样保留）
/home/ubuntu/.ilink-hub-bridge/
├── profiles/
│   ├── local-VM-8-2-ubuntu-ilink-hub-bridge.yaml  # claude-code bridge（复用旧 token，Hub 名不变）
│   └── tcloud-codebuddy.yaml                       # codebuddy bridge（模型 hy3-ioa）
└── credentials/
    ├── local-VM-8-2-ubuntu-ilink-hub-bridge.json   # 由 manager 注册时自动写入
    └── tcloud-codebuddy.json                        # 由 manager 注册时自动写入
```

## 关键约定 / 坑

1. **executor 名**：manager 模式委托给 agentproc crate，注册名是 `codebuddy`（不是 `codebuddy-code`
   别名）。`tcloud-codebuddy.yaml` 用 `executor: codebuddy` + `command: codebuddy`。
2. **凭据复用**：claude profile 文件名 stem 故意等于旧单 bridge 的 register_name，从而复用同 token，
   `/use` 目标零变化。
3. **同名冲突**：本机 hub 同时被 Mac manager 连着，`ilink-hub-codebuddy` 这名被 Mac 占了，所以本机用
   唯一名 `tcloud-codebuddy`，避免 409。

## 微信切换

```
/use tcloud-codebuddy
```
