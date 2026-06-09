# 安装

::: tip 不想用终端？
**[桌面应用](#桌面应用-tauri)** 是最简单的方式，双击安装，无需命令行。直接跳到页面底部查看。
:::

---

## 方式一：桌面应用（推荐新手）{#desktop}

与命令行版本功能相同，提供图形界面，首次绑定微信时会弹出二维码窗口。

### 下载

在 **[GitHub Releases](https://github.com/jeffkit/ilink-hub/releases)** 最新版本的 Assets 里，找到以 `ilink-hub-desktop-` 开头的文件：

| 文件 | 适用系统 |
|------|---------|
| `ilink-hub-desktop-macos-aarch64.dmg` | macOS Apple Silicon（M1/M2/M3/M4） |
| `ilink-hub-desktop-macos-x86_64.dmg` | macOS Intel |
| `ilink-hub-desktop-windows-x86_64.msi` | Windows 10/11 64 位 |
| `ilink-hub-desktop-linux-x86_64.deb` | Ubuntu / Debian 等 |

**不知道自己是哪种 Mac？** 点击苹果菜单 → 「关于本机」，芯片栏写 Apple M* 选 aarch64，写 Intel 选 x86_64。

### 安装注意

- **macOS**：当前版本未做 Apple 公证。首次打开若被拦截，请右键点击应用 → 「打开」，或前往「系统设置 → 隐私与安全性」允许运行。
- **Windows**：按 MSI 向导安装；若 SmartScreen 提示未知发布者，选「仍要运行」。
- **Linux**：`sudo apt install ./ilink-hub-desktop-linux-x86_64.deb`

### 使用桌面版时接 Bridge

桌面版只负责在本机运行 Hub（默认监听 `127.0.0.1:8765`）。如果你还想接 Claude Code / Codex 等命令行工具，需要另外安装 `ilink-hub-bridge`，参考[本地 CLI Bridge 使用指引](/bridge/USAGE)，并设置 `WEIXIN_BASE_URL=http://127.0.0.1:8765`。

---

## 方式二：Homebrew（macOS 命令行推荐）

```bash
brew tap jeffkit/tap
brew install ilink-hub
```

验证安装（会同时安装 `ilink-hub` 和 `ilink-hub-bridge` 两个命令）：

```bash
ilink-hub --version
ilink-hub-bridge --version
```

::: details 安装后找不到 `ilink-hub-bridge`？
先更新 Homebrew 再升级：
```bash
brew update
brew upgrade ilink-hub
```
:::

升级到新版本：

```bash
brew upgrade ilink-hub
```

---

## 方式三：预编译二进制（无需 Rust）

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
ilink-hub-bridge --version
```

---

## 方式四：Docker

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

## 方式五：Cargo（需要 Rust 工具链）

```bash
cargo install ilink-hub
```

安装后两个命令（`ilink-hub` 与 `ilink-hub-bridge`）都在 `~/.cargo/bin/`，确保该目录在 PATH 中。

---

## 方式六：从源码编译

```bash
git clone https://github.com/jeffkit/ilink-hub.git
cd ilink-hub
cargo build --release
# 二进制在 target/release/ilink-hub 和 target/release/ilink-hub-bridge
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
