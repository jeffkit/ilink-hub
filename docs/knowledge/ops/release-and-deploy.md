---
type: Guide
title: 发布与部署规范
description: ilink-hub / ilink-hub-bridge 的发布与部署规范——三档路径（本机 brew 快速调试 / mac-fast patch 发布 / 完整多平台发布）及远程 Hub 部署。
resource: docs/knowledge/ops/release-and-deploy.md
tags: [ops, release, deploy, brew, ci]
timestamp: 2026-06-21T19:42:00+08:00
---

# 发布与部署规范

> **铁律**：本地 hub/bridge 部署**必须经 brew 管理**（`/opt/homebrew/bin`），并**递增版本号**。
> **禁止**用 `deploy-local-mac.sh` 把二进制裸拷到 `~/.local/bin` 覆盖运行 —— 那会与
> launchd / brew services 指向的 brew 路径脱节，且无版本可追溯。

launchd 服务 `com.ilink-hub.bridge-manager` 的 plist 指向 `/opt/homebrew/bin/ilink-hub-bridge`
（brew 安装路径），因此任何本地部署都要落到这个 brew 路径上，重载后才会生效。

## 三档路径

| 场景 | 路径 | 工具 | 耗时 | 产物位置 |
|------|------|------|------|---------|
| 本机快速调试（patch） | 方案 2 | `scripts/deploy-local-brew.sh` | ~1 min | 仅本机 brew Cellar |
| patch「发出去」（需他人/多机可用） | 方案 1 | `release-mac-fast.yml`（`v*-mac` tag 或手动 dispatch） | ~5 min | GitHub `mac-latest` 预发布 + 公共 tap |
| minor / major 正式发布 | 完整 | `release.yml`（`vX.Y.0` tag） | 30+ min | 全平台 GitHub Release + crates.io + 公共 tap + Docker |

> 选择原则：第 3 位（patch）变更日常调试走方案 2；需要推给其他机器或公共 tap 时走方案 1；
> 含破坏性变更或对外版本走完整发布。

### 方案 2：本机 build → brew（零 CI，最快）

```bash
# 1. 递增 Cargo.toml 的 version（patch 位）
# 2. 本地构建并经 brew 安装到 /opt/homebrew/bin，顺带重载 bridge-manager
ILINK_RELOAD_LAUNCHD=1 scripts/deploy-local-brew.sh
```

脚本流程：本地 `cargo build --release` → ad-hoc 重签（避免 macOS AMFI `killed: 9`）→
临时把 `jeffkit/tap` 的 formula 改成 `file://` 指向本地产物 → `brew reinstall` → 还原 formula。
产物只在本机，公共 tap 不受影响。

### 方案 1：mac-fast 轻量 CI（patch 规范发布）

```bash
# 递增 Cargo.toml version 后，提交并打 -mac 后缀 tag：
git tag v0.2.2-mac && git push origin v0.2.2-mac
# 或在 GitHub Actions 手动运行 "Release (mac-fast)"，填 version
```

只构建 macOS（arm64+x86）的 hub+bridge，上传到滚动预发布 `mac-latest`，并更新公共 tap。
用固定 tag `mac-latest`（非 `v*`），**不会**触发完整 `release.yml`。安装：

```bash
brew update && brew upgrade jeffkit/tap/ilink-hub
```

### 完整发布：release.yml（minor / major）

```bash
# 递增 Cargo.toml version（minor/major），更新 CHANGELOG，提交后：
git tag v0.3.0 && git push origin v0.3.0
```

全平台 hub+bridge 二进制、crates.io、GitHub Release、自动更新公共 tap、Docker（独立 workflow）。

> **CI 解耦**：`create-release` 与 `update-homebrew-tap` 只依赖 `build-native` + `publish-crates`，
> **不再**依赖 `build-desktop`（Tauri）。Desktop 安装包为 best-effort（`continue-on-error`），
> 其失败不会再跳过 Release 和 brew tap 更新 —— 这曾是 brew 长期停在旧版本的根因。

## 远程 Hub 部署（tcloud_gz / systemd）

远程 Hub（SSH 别名 `tcloud_gz`，地址 `43.138.149.34`，systemd 单元 `ilink-hub`）。
本机直连其 `8765` 端口（**不再走 SSH 隧道转发**）。

> ⚠️ **生产 hub 唯一实例**：生产 hub **只在 `tcloud_gz`（`43.138.149.34`）**。切勿在其他机器另起
> hub 共用同一 `ILINK_TOKEN`——iLink 对单个 bot 只允许一个活跃会话，多实例会互抢，被抢者持续
> 报 `-14 session timeout` 且收不到任何消息。

### ⚠️ 交叉编译避坑：优先在服务器上直接编译

> **不要在本机用 `cross` 或 `musl` 交叉编译**，有以下两个已知坑：
>
> 1. **`cross`（Docker 容器）**：首次或缓存失效后需从头编译所有依赖，在 QEMU 仿真下
>    耗时极长（30 分钟以上）；且曾出现 QEMU 下链接器 SIGSEGV 导致编译失败的问题。
>    只有当 `target/x86_64-unknown-linux-gnu/` 缓存热时才快；缓存一旦冷，不如服务器直编。
> 2. **`musl` 静态编译**：部分 crate 对 musl libc 兼容性存在问题，不稳定。
>
> **推荐做法：rsync 源码到服务器，直接 `cargo build`**（服务器已安装 Rust，原生编译
> 约 1-2 分钟，增量编译仅需数秒）。

```bash
# 0. 确认服务器有 Rust（首次确认即可）
ssh tcloud_gz "~/.cargo/bin/cargo --version"

# 1. 备份生产库（迁移不可逆，务必先备份）
ssh tcloud_gz "python3 -c \"
import sqlite3, shutil, datetime
ts = datetime.datetime.now().strftime('%Y%m%d-%H%M%S')
shutil.copy('/var/lib/ilink-hub/ilink-hub.db', f'/var/lib/ilink-hub/ilink-hub.db.bak.{ts}')
print('backup ok:', ts)
\""

# 2. rsync 源码到服务器（migrations 必须一起同步，include_str! 引用的是编译时路径）
cd /path/to/ilink-hub
rsync -av src/ tcloud_gz:/home/ubuntu/ilink-hub-build/src/
rsync -av migrations/ tcloud_gz:/home/ubuntu/ilink-hub-build/migrations/
rsync -av Cargo.toml Cargo.lock tcloud_gz:/home/ubuntu/ilink-hub-build/

# 3. 在服务器上编译（约 1 分钟全量，增量数秒）
ssh tcloud_gz "cd /home/ubuntu/ilink-hub-build && ~/.cargo/bin/cargo build --release --bin ilink-hub 2>&1 | tail -3"

# 4. 停服 → 替换二进制 → 启服（必须先 stop 再替换，否则报 "Text file busy"）
ssh tcloud_gz "
  sudo systemctl stop ilink-hub
  sudo cp /home/ubuntu/ilink-hub-build/target/release/ilink-hub /opt/ilink-hub/ilink-hub
  sudo systemctl start ilink-hub
  sleep 2 && systemctl status ilink-hub --no-pager | tail -5
"

# 5. 健康检查
curl -s http://43.138.149.34:8765/health   # → ok
```

### 已知环境变量（服务器 systemd 单元必须包含）

服务器 `/etc/systemd/system/ilink-hub.service` 的 `[Service]` 段需要以下变量，
缺任何一个都会导致 Hub 启动失败：

```ini
Environment=RUST_LOG=info
Environment=DATABASE_URL=sqlite:///var/lib/ilink-hub/ilink-hub.db
Environment=ILINK_TOKEN=<bot_token>
Environment=ILINK_HUB_MASTER_KEY=<32字节base64>   # M2安全变更引入，缺少则启动即崩
Environment=ILINK_ADMIN_TOKEN=<admin_token>        # 如启用了管理 API
```

> `ILINK_HUB_MASTER_KEY` 用于加密数据库中的 `bot_credentials`。若丢失此 key，
> 需清空 `bot_credentials` 表并重新生成 key（Hub 会在启动时从 `ILINK_TOKEN` 重新加密写入）：
>
> ```bash
> # 生成新 key
> NEW_KEY=$(openssl rand -base64 32)
> # 清空旧加密数据（否则用新 key 解密会失败）
> python3 -c "import sqlite3; c=sqlite3.connect('/var/lib/ilink-hub/ilink-hub.db'); c.execute('DELETE FROM bot_credentials'); c.commit()"
> # 然后把 $NEW_KEY 写入 service 文件并 systemctl daemon-reload
> ```

### 替换二进制时的 "Text file busy" 问题

直接 `cp` 到正在运行的二进制会报 `Text file busy`。**必须先 stop 后替换**：

```bash
# 错误做法（会报错）：
sudo cp new-binary /opt/ilink-hub/ilink-hub   # ❌ 服务运行时

# 正确做法：
sudo systemctl stop ilink-hub
sudo cp new-binary /opt/ilink-hub/ilink-hub   # ✅
sudo systemctl start ilink-hub
```

## 提交前检查清单（部署相关）

- [ ] 已递增 `Cargo.toml` 的 `version`，并 `cargo build` 同步 `Cargo.lock`
- [ ] 本机调试用方案 2（brew），**绝不**裸拷到 `~/.local/bin`
- [ ] patch 对外用方案 1（`v*-mac`），minor/major 用完整 `release.yml`（`vX.Y.Z`）
- [ ] 远程 Hub 部署前已备份生产 SQLite 库
- [ ] **不要**用 `cross` 或 `musl` 交叉编译 Hub，改为 rsync 源码到服务器直接编译
- [ ] `systemd` service 文件包含所有必需的 `Environment=` 变量（见上方清单）
- [ ] 替换二进制前先 `sudo systemctl stop ilink-hub`

## 相关文档

- [部署安全加固](deployment-hardening.md)
- [环境变量配置](../api/configuration.md)
