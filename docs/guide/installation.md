# 安装

---

## 方式一：Homebrew（macOS 命令行推荐）

```bash
brew tap jeffkit/tap
brew install ilink-hub
```

验证安装：

```bash
ilink-hub --version
```

> 自 `0.4.0` 起，`jeffkit/tap/ilink-hub` formula 仅安装 Hub 服务本体（`ilink-hub`）。
> bridge（原 `ilink-hub-bridge`）已拆到独立项目 [im-agentproc](https://github.com/jeffkit/im-agentproc)，
> 由其独立的 Homebrew formula 提供。

升级到新版本：

```bash
brew upgrade ilink-hub
```

---

## 方式二：预编译二进制（无需 Rust）

直接下载对应平台的可执行文件，不需要安装任何运行环境。

### macOS

```bash
# Apple Silicon（M1/M2/M3/M4）
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-aarch64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/

# Intel 芯片
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-macos-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

### Linux

```bash
# x86_64（大多数服务器和桌面）
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

### Windows

从 [Releases 页面](https://github.com/jeffkit/ilink-hub/releases) 下载 `ilink-hub-windows-x86_64.exe`，重命名为 `ilink-hub.exe` 放到 PATH 中的目录。

### 验证安装

```bash
ilink-hub --version
```

---

## 方式三：Docker

```bash
docker pull ghcr.io/jeffkit/ilink-hub:latest

docker run -it --rm \
  -p 8765:8765 \
  -v ilink-hub-data:/data \
  -e DATABASE_URL=sqlite:/data/ilink-hub.db \
  ghcr.io/jeffkit/ilink-hub:latest serve
```

首次启动会在日志里打印登录二维码，用 `docker logs -f <容器名>` 查看。详细部署见 [Docker 部署指南](/deployment/docker)。

---

## 方式四：Cargo（需要 Rust 工具链）

默认仅启用 SQLite 支持以减少编译耗时。若需要启用 PostgreSQL 或 MySQL 驱动支持，需要显式指定 `--features` 参数：

```bash
# 仅 SQLite 支持（默认）
cargo install ilink-hub

# 启用 PostgreSQL 支持
cargo install ilink-hub --features postgres

# 启用 MySQL 支持
cargo install ilink-hub --features mysql
```

安装后 `ilink-hub` 在 `~/.cargo/bin/`，确保该目录在 PATH 中。bridge（原 `ilink-hub-bridge`）已拆到独立项目 [im-agentproc](https://github.com/jeffkit/im-agentproc)，如需本地 CLI bridge 请单独安装它。

---

## 方式五：从源码编译

```bash
git clone https://github.com/jeffkit/ilink-hub.git
cd ilink-hub

# 默认仅编译 SQLite 驱动
cargo build --release

# 启用特定数据库驱动支持
cargo build --release --features postgres
cargo build --release --features mysql
# 或启用所有驱动支持
cargo build --release --all-features

# 二进制在 target/release/ilink-hub（bridge 已拆到独立项目 im-agentproc）
```

---

## 系统要求

| 平台 | 最低版本 |
|------|---------|
| macOS | 11.0+ |
| Windows | Windows 10 64-bit |
| Linux | glibc 2.17+（CentOS 7+、Ubuntu 16.04+） |
| Docker | 20.10+ |

::: details Linux 报错「glibc version not found」？
你的系统 glibc 版本低于 2.17。推荐改用 Docker 方式，不依赖宿主 glibc 版本。
:::
