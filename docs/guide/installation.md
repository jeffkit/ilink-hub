# 安装

## 方式一：Homebrew（macOS 推荐）

macOS 用户推荐使用 Homebrew 安装，自动处理安装和后续更新：

```bash
brew tap jeffkit/tap
brew install ilink-hub
```

验证安装：

```bash
ilink-hub --version
ilink-hub-bridge --version
```

从 **v0.1.7** 起（**v0.1.8** 起包含凭证保护与 `--force-register` 等 bridge 更新；**v0.1.9** 起为多后端默认展示名脚注与引用路由相关改进），同一 formula 会安装 **`ilink-hub`** 与 **`ilink-hub-bridge`** 两个命令（均来自同一 GitHub Release 标签）。若 `brew install` 后没有 `ilink-hub-bridge`，请先执行 `brew update` 再升级：

```bash
brew update
brew upgrade ilink-hub
```

升级到新版本：

```bash
brew upgrade ilink-hub
```

---

## 方式二：预编译二进制

无需 Rust 环境，直接下载对应平台的二进制文件。

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
# x86_64（大多数服务器/桌面）
curl -Lo ilink-hub https://github.com/jeffkit/ilink-hub/releases/latest/download/ilink-hub-linux-x86_64
chmod +x ilink-hub
sudo mv ilink-hub /usr/local/bin/
```

### Windows

从 [Releases 页面](https://github.com/jeffkit/ilink-hub/releases) 下载 `ilink-hub-windows-x86_64.exe`，重命名为 `ilink-hub.exe` 并放到 PATH 中的目录。

### 验证安装

```bash
ilink-hub --version
ilink-hub-bridge --version
```

---

## 方式二：Cargo 安装（需要 Rust）

如果你已经安装了 Rust 工具链：

```bash
cargo install ilink-hub
```

安装后二进制在 `~/.cargo/bin/`（含 `ilink-hub` 与 `ilink-hub-bridge`），确保该目录在 PATH 中。

---

## 方式三：Docker

```bash
# 拉取最新镜像（支持 amd64 和 arm64）
docker pull ghcr.io/jeffkit/ilink-hub:latest

# 镜像默认 CMD 为 serve；首次启动会在日志里打印 iLink 登录二维码
docker run -it --rm \
  -p 8765:8765 \
  -v ilink-hub-data:/data \
  -e DATABASE_URL=sqlite:/data/ilink-hub.db \
  ghcr.io/jeffkit/ilink-hub:latest serve
```

详细 Docker 部署见 [Docker 部署指南](/deployment/docker)。

---

## 方式四：从源码编译

```bash
git clone https://github.com/jeffkit/ilink-hub.git
cd ilink-hub
cargo build --release
# 二进制在 target/release/ilink-hub
```

---

## 系统要求

| 平台 | 最低版本 |
|------|--------|
| Linux | glibc 2.17+（CentOS 7+、Ubuntu 16.04+） |
| macOS | 11.0+ |
| Windows | Windows 10 64-bit |
| Docker | 20.10+ |
