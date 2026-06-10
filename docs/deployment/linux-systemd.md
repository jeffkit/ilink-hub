# Linux / VPS 部署（systemd）

在没有 Docker 的 VPS 或私有服务器上，可以将 iLink Hub 编译为原生二进制，交由 systemd 守护运行。

## 前提条件

- Ubuntu / Debian / CentOS 等主流 Linux 发行版
- 已安装 Rust 工具链（`rustup`）
- 外网可访问微信 iLink 接口（`ilinkai.weixin.qq.com`）

::: tip 大陆服务器网络
若你的服务器在中国大陆，`ilinkai.weixin.qq.com` 通常可直连；若部署在香港或海外，请确认出站网络是否可以访问该域名，否则 QR 登录轮询会超时。
:::

## 第一步：编译二进制

在服务器上克隆仓库并编译（如果服务器是 x86_64，无需任何交叉编译配置）：

```bash
# 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 克隆并编译
git clone https://github.com/jeffkit/ilink-hub.git
cd ilink-hub
cargo build --release --bin ilink-hub

# 安装到系统路径
sudo cp target/release/ilink-hub /opt/ilink-hub/ilink-hub
sudo chmod +x /opt/ilink-hub/ilink-hub
```

## 第二步：创建数据目录

```bash
sudo mkdir -p /var/lib/ilink-hub
sudo chown $USER:$USER /var/lib/ilink-hub
```

## 第三步：创建 systemd 服务

```bash
sudo tee /etc/systemd/system/ilink-hub.service << 'EOF'
[Unit]
Description=iLink Hub Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
Group=ubuntu
Environment=RUST_LOG=info
Environment=DATABASE_URL=sqlite:///var/lib/ilink-hub/ilink-hub.db
# 推荐：设置管理 Token，保护注册接口
# Environment=ILINK_ADMIN_TOKEN=your-secret-here
# 可选：填写已有微信 Token，跳过扫码（见下方说明）
# Environment=ILINK_TOKEN=your-weixin-token

ExecStart=/opt/ilink-hub/ilink-hub serve --addr 0.0.0.0:8765
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable ilink-hub
sudo systemctl start ilink-hub
```

## 第四步：首次微信登录

### 方式一：使用已有 Token（推荐）

若你已经在本机或其他服务器上登录过微信，可以从数据库中取出 Token，通过环境变量直接传给新服务器，跳过扫码流程：

```bash
# 在已登录的旧机器上取出 Token（SQLite 示例）
sqlite3 ~/.ilink-hub/ilink-hub.db \
  "SELECT value FROM settings WHERE key='ilink_token';"

# 将取出的 Token 填入 /etc/systemd/system/ilink-hub.service
# Environment=ILINK_TOKEN=your-weixin-token

sudo systemctl daemon-reload && sudo systemctl restart ilink-hub
sudo systemctl status ilink-hub
```

### 方式二：在服务器终端扫码

若服务器终端支持 UTF-8（大多数 SSH 终端都支持），可以先以前台方式启动，扫码后再切换后台：

```bash
# 前台运行（扫码后 Ctrl+C 停止）
/opt/ilink-hub/ilink-hub serve --addr 0.0.0.0:8765

# 扫码成功后，Token 已写入数据库，再以服务方式启动
sudo systemctl start ilink-hub
```

### 方式三：本地代扫（适合无终端显示的环境）

在**本地电脑**执行 `ilink-hub login --hub-url http://<server-ip>:8765` 拉取 QR 码，扫码后 Token 写入服务器数据库。

## 查看运行日志

```bash
# 实时日志
sudo journalctl -u ilink-hub -f

# 最近 100 行
sudo journalctl -u ilink-hub -n 100
```

## 健康检查

```bash
curl http://localhost:8765/health
# → {"status":"ok","upstream":"connected","clients":{"online":0,"total":0}}
```

## 更新到新版本

```bash
cd ilink-hub
git pull
cargo build --release --bin ilink-hub
sudo systemctl stop ilink-hub
sudo cp target/release/ilink-hub /opt/ilink-hub/ilink-hub
sudo systemctl start ilink-hub
```

## 从 macOS 交叉编译部署（服务器无 cargo 时）

若服务器上没有 Rust 工具链（或安装麻烦），可以在 **macOS (arm64)** 本地交叉编译出 x86_64 Linux 静态二进制，再 scp 上传。

> **背景**：直接 `cargo build --release` 产出的是 Mach-O arm64 二进制，上传到 x86_64 Linux 后 systemd 启动报 `status=203/EXEC`，无法执行。必须使用 musl 交叉编译。

### 前置（一次性）

```bash
# 安装 musl cross toolchain
brew install musl-cross

# 添加 musl target
rustup target add x86_64-unknown-linux-musl
```

### 每次部署

```bash
# 1. 交叉编译 Linux 静态链接二进制
MUSL_PREFIX=$(brew --prefix musl-cross)/bin
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$MUSL_PREFIX/x86_64-linux-musl-gcc" \
CC_x86_64_unknown_linux_musl="$MUSL_PREFIX/x86_64-linux-musl-gcc" \
  cargo build --release --target x86_64-unknown-linux-musl

# 2. 上传并替换（必须先 stop，运行中的二进制不能直接覆盖会报 Text file busy）
scp target/x86_64-unknown-linux-musl/release/ilink-hub <server>:/tmp/ilink-hub-new
ssh <server> "sudo systemctl stop ilink-hub && \
  sudo cp /tmp/ilink-hub-new /opt/ilink-hub/ilink-hub && \
  sudo chmod +x /opt/ilink-hub/ilink-hub && \
  sudo systemctl start ilink-hub"
```

musl 产出的是静态链接二进制，无 glibc 版本依赖，可在任意 x86_64 Linux 上运行。

## 防火墙配置（可选）

若服务器只允许 Bridge 通过 SSH 隧道访问，**不需要**对外开放 8765 端口。若需要公网直连（例如 Recursive / OpenClaw 客户端直接访问），请开放端口：

```bash
# Ubuntu UFW
sudo ufw allow 8765/tcp

# CentOS firewalld
sudo firewall-cmd --permanent --add-port=8765/tcp
sudo firewall-cmd --reload
```

生产环境建议在 8765 前配置 Nginx 反向代理并启用 HTTPS，参见 [安全建议](/deployment/security)。
