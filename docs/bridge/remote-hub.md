# 连接远程 Hub

默认情况下，`ilink-hub-bridge` 假设 Hub 运行在同一台机器（`http://127.0.0.1:8765`）。但实际上，Bridge 可以连接任意位置的 Hub——本地网络、公司服务器、VPS，只需指向正确的地址即可。

## 为什么需要远程 Hub？

| 场景 | 说明 |
|------|------|
| Hub 在服务器上，Bridge 在本机 | 本机 Claude Code 处理消息，服务器常驻微信连接 |
| 多台电脑共享同一个微信连接 | 每台机器各跑一个 Bridge，指向同一个 Hub |
| 不想在本机跑 Hub | 服务器有稳定 IP、无需维护本地服务 |

## 方式一：Hub 公网可达

若 Hub 服务器已配置防火墙允许 8765（或通过 Nginx 映射到 443），直接指定地址：

```bash
ilink-hub-bridge --hub-url http://your-server:8765
```

或使用环境变量（`.env` / shell 配置）：

```bash
export WEIXIN_BASE_URL=http://your-server:8765
ilink-hub-bridge
```

::: tip 安全建议
公网暴露 Hub 时，强烈建议配置 `ILINK_ADMIN_TOKEN`，并通过 Nginx + HTTPS 对外提供服务，参见 [安全建议](/deployment/security)。
:::

## 方式二：SSH 端口转发（Hub 不对外开放）

若 Hub 服务器 8765 端口**未对外开放**，可用 SSH 将本地端口转发到远程服务器：

```bash
# 将本机 18765 端口转发到远程服务器的 8765
ssh -N -L 18765:localhost:8765 user@your-server
```

然后 Bridge 连接本地代理端口：

```bash
ilink-hub-bridge --hub-url http://localhost:18765
```

或使用 Bridge Manager：

```bash
ilink-hub-bridge manager --hub-url http://localhost:18765
```

::: info SSH 配置建议
在 `~/.ssh/config` 中定义主机别名，SSH 命令会更简洁：

```
Host myserver
    HostName 1.2.3.4
    User ubuntu
    IdentityFile ~/.ssh/id_ed25519
    ServerAliveInterval 30
    ServerAliveCountMax 3
```

这样使用 `ssh -N -L 18765:localhost:8765 myserver` 即可连接。
:::

## 持久化运行 Bridge（macOS，launchd）

在 macOS 上，推荐用 launchd 确保 Bridge Manager 在登录后自动启动、异常退出后自动重启。

> **推荐：直连**。当 Hub 公网可达（方式一）时，Bridge Manager 直接连远程 `8765`，**只需一个服务**、无需 SSH 隧道。本节即按此方式编写；若 Hub 不对外开放，见文末 [可选：SSH 隧道持久化](#可选-ssh-隧道持久化)。

### 创建日志目录

```bash
mkdir -p ~/ilink-logs
```

### Bridge Manager launchd 服务（直连）

创建 `~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist`。程序路径用 **Homebrew 安装路径** `/opt/homebrew/bin/ilink-hub-bridge`（见 [发布与部署规范](../knowledge/ops/release-and-deploy.md)，本地部署一律经 brew，不要裸拷 `~/.local/bin`）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.ilink-hub.bridge-manager</string>
    <key>ProgramArguments</key>
    <array>
        <string>/opt/homebrew/bin/ilink-hub-bridge</string>
        <string>manager</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>5</integer>
    <key>StandardOutPath</key>
    <string>/Users/你的用户名/ilink-logs/bridge-manager.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/你的用户名/ilink-logs/bridge-manager-error.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key><string>info</string>
        <key>HOME</key><string>/Users/你的用户名</string>
        <!-- 直连远程 Hub：指向你的公网 Hub 地址（不再经 SSH 隧道） -->
        <key>WEIXIN_BASE_URL</key><string>http://your-server:8765</string>
        <!-- 包含 claude、node 等工具的完整 PATH -->
        <key>PATH</key>
        <string>/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/Users/你的用户名/.cargo/bin:/Users/你的用户名/.local/bin</string>
    </dict>
</dict>
</plist>
```

::: warning PATH 配置很重要
launchd 服务不继承 shell 的 `PATH`，必须在 `EnvironmentVariables` 中显式列出所有工具的路径。常见路径：
- Homebrew（Apple Silicon）：`/opt/homebrew/bin`
- Homebrew（Intel Mac）：`/usr/local/bin`
- Cargo：`~/.cargo/bin`
- Claude Code：`/opt/homebrew/bin/claude`（通过 Homebrew 安装）
:::

### 加载服务

```bash
# 加载 Bridge Manager（直连远程 Hub，无需 SSH 隧道）
launchctl load ~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist

# 查看状态
launchctl list | grep ilink-hub
```

> 升级 Bridge 后重载服务（`bootout` + `bootstrap` 会让 manager 及其子 Bridge 全部用新二进制重启）：
>
> ```bash
> uid=$(id -u)
> launchctl bootout  "gui/$uid/com.ilink-hub.bridge-manager"
> launchctl bootstrap "gui/$uid" ~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist
> ```

### 常用管理命令

```bash
# 查看运行状态（有 PID 表示运行中，最后一列是上次退出码）
launchctl list | grep ilink-hub

# 查看 Bridge 日志
tail -f ~/ilink-logs/bridge-manager.log

# 手动停止 / 启动
launchctl unload ~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist
launchctl load  ~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist

# 登录时自动启动（已包含在 plist 的 RunAtLoad=true）
# 重启系统后 launchd 会自动拉起服务
```

### 验证 Bridge 在线

```bash
# 查看所有已注册客户端的在线状态（直连远程 Hub）
curl http://your-server:8765/hub/clients
```

## 可选：SSH 隧道持久化

仅当 Hub 的 `8765` **不对外开放**时才需要本节。此时额外再加一个 SSH 隧道服务，并把上面 Bridge Manager 的 `WEIXIN_BASE_URL` 改为 `http://localhost:18765`（去掉直连地址）。

创建 `~/Library/LaunchAgents/com.ilink-hub.ssh-tunnel.plist`：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.ilink-hub.ssh-tunnel</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/ssh</string>
        <string>-N</string>
        <string>-o</string><string>ExitOnForwardFailure=yes</string>
        <string>-o</string><string>ServerAliveInterval=30</string>
        <string>-o</string><string>ServerAliveCountMax=3</string>
        <string>-L</string>
        <string>18765:localhost:8765</string>
        <string>myserver</string>  <!-- 替换为你的 SSH 主机别名或地址 -->
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key>
    <string>/Users/你的用户名/ilink-logs/ssh-tunnel.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/你的用户名/ilink-logs/ssh-tunnel-error.log</string>
</dict>
</plist>
```

加载顺序：先隧道、再 Bridge Manager。

```bash
launchctl load ~/Library/LaunchAgents/com.ilink-hub.ssh-tunnel.plist
sleep 3   # 等隧道建立
launchctl load ~/Library/LaunchAgents/com.ilink-hub.bridge-manager.plist
```

## 持久化运行（Linux，systemd）

在 Linux 服务器上运行 Bridge Manager（连接远程 Hub），同样可以使用 systemd：

```ini
# /etc/systemd/system/ilink-hub-bridge.service
[Unit]
Description=iLink Hub Bridge Manager
After=network-online.target

[Service]
Type=simple
User=ubuntu
Environment=RUST_LOG=info
Environment=WEIXIN_BASE_URL=http://remote-hub:8765
ExecStart=/usr/local/bin/ilink-hub-bridge manager
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now ilink-hub-bridge
sudo journalctl -u ilink-hub-bridge -f
```

## 排查连接问题

```bash
# 确认 Hub 可达（直连）
curl http://your-server:8765/health

# 仅 SSH 隧道方式才需要：检查隧道是否建立
ss -tlnp | grep 18765      # Linux
netstat -an | grep 18765   # macOS

# 测试 Bridge Manager 的 PATH 是否包含所需工具
/usr/bin/env -i PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin" \
  which claude
```
