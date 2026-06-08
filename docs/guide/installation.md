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

从 **v0.1.7** 起（**v0.1.8** 起包含凭证保护与 `--force-register` 等 bridge 更新；**v0.1.9** 起为多后端默认展示名脚注与引用路由相关改进；**v0.1.10** 起 GitHub Release 附带 Tauri 桌面安装包 `ilink-hub-desktop-*`），同一 formula 会安装 **`ilink-hub`** 与 **`ilink-hub-bridge`** 两个命令（均来自同一 GitHub Release 标签）。若 `brew install` 后没有 `ilink-hub-bridge`，请先执行 `brew update` 再升级：

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

## 方式五：桌面应用（Tauri） {#desktop}

与命令行 `ilink-hub serve` 使用同一套运行时（默认监听 `127.0.0.1:8765`，首次绑定微信时窗口内展示二维码）。数据库位于各系统用户数据目录下的 `ilink-hub-desktop/ilink-hub.db`，与 CLI 默认的当前目录 `./ilink-hub.db` 不同。

### 从哪里下载

在 **[GitHub Releases](https://github.com/jeffkit/ilink-hub/releases)** 的对应版本中，查找以下 **Assets**（文件名以 `ilink-hub-desktop-` 开头）。自 **推送 `v*` 版本 tag 并完成 Release 流水线**起，这些文件会随 `ilink-hub` 预编译二进制一并由 CI 上传；若某个历史版本没有这些条目，说明该 tag 发布时尚未包含桌面构建，请改用 [预编译二进制](#方式二预编译二进制) 或 [从源码打包桌面包](#自行从源码打包安装包)。

| 文件 | 适用环境 |
|------|----------|
| `ilink-hub-desktop-macos-aarch64.dmg` | macOS Apple Silicon（M 系列） |
| `ilink-hub-desktop-macos-x86_64.dmg` | macOS Intel |
| `ilink-hub-desktop-windows-x86_64.msi` | Windows 64 位 |
| `ilink-hub-desktop-linux-x86_64.deb` | Debian / Ubuntu 等 x86_64 |

使用 **`…/releases/latest/download/<文件名>`** 可始终指向当前最新版（与 CLI 二进制下载方式一致）。

### 安装后说明

- **macOS**：当前 Release **未做 Apple 公证**。首次打开若被拦截，请在访达中对应用 **右键 → 打开**，或在「隐私与安全性」中允许运行。需要分发级公证时，维护者需在 CI 中配置证书与 notarization（成本与密钥管理高于 CLI）。
- **Windows**：按 MSI 向导安装；若 SmartScreen 提示未知发布者，选择「仍要运行」或改用 CLI/Docker。
- **Linux**：例如 `sudo apt install ./ilink-hub-desktop-linux-x86_64.deb`（路径以你下载位置为准）。依赖系统已安装的 WebKitGTK 等，与 [Tauri Linux 前置依赖](https://v2.tauri.app/start/prerequisites/) 一致。
- **与 ilink-hub-bridge**：桌面版只负责在本机拉起 Hub；接 Claude Code / Codex 等仍按 [本地 CLI Bridge 使用指引](/bridge/USAGE) 安装 `ilink-hub-bridge` 并配置 `WEIXIN_BASE_URL=http://127.0.0.1:8765`（若改过端口则写实际地址）。

### 自行从源码打包安装包

若 Releases 尚未包含桌面包，或你需要本地修改后再安装：

```bash
cd desktop/ilink-hub-desktop
npm install
npm run tauri build
```

产物目录见 [`desktop/ilink-hub-desktop/README.md`](https://github.com/jeffkit/ilink-hub/blob/main/desktop/ilink-hub-desktop/README.md)。

---

## 系统要求

| 平台 | 最低版本 |
|------|--------|
| Linux | glibc 2.17+（CentOS 7+、Ubuntu 16.04+） |
| macOS | 11.0+ |
| Windows | Windows 10 64-bit |
| Docker | 20.10+ |
