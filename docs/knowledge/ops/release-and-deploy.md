---
type: Guide
title: 发布与部署规范
description: ilink-hub / ilink-hub-bridge 的发布与部署规范——三档路径（本机 brew 快速调试 / mac-fast patch 发布 / 完整多平台发布）及远程 Hub 部署。
resource: docs/knowledge/ops/release-and-deploy.md
tags: [ops, release, deploy, brew, ci]
timestamp: 2026-06-18T18:30:00+08:00
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

远程 Hub（`43.138.149.34`，systemd 单元 `ilink-hub`）用 musl 静态二进制更新。本机直连其 `8765`
端口（**不再走 SSH 隧道转发**）。

```bash
# 1. 备份生产库（迁移不可逆，务必先备份）
ssh tcloud_gz 'TS=$(date +%Y%m%d-%H%M%S); sqlite3 /var/lib/ilink-hub/ilink-hub.db ".backup /var/lib/ilink-hub/ilink-hub.db.bak.$TS"'

# 2. musl 交叉编译（macOS arm64 → linux x86_64 静态）
MUSL_PREFIX=$(brew --prefix musl-cross)/bin
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$MUSL_PREFIX/x86_64-linux-musl-gcc" \
CC_x86_64_unknown_linux_musl="$MUSL_PREFIX/x86_64-linux-musl-gcc" \
  cargo build --release --target x86_64-unknown-linux-musl --bin ilink-hub

# 3. 上传 + 原子替换 + 重启（停服会有几秒微信中断，并在启动时跑挂起的迁移）
scp target/x86_64-unknown-linux-musl/release/ilink-hub tcloud_gz:/tmp/ilink-hub.new
ssh tcloud_gz 'set -e; TS=$(date +%Y%m%d-%H%M%S); chmod +x /tmp/ilink-hub.new; \
  sudo systemctl stop ilink-hub; \
  sudo cp /opt/ilink-hub/ilink-hub /opt/ilink-hub/ilink-hub.bak.$TS; \
  sudo mv /tmp/ilink-hub.new /opt/ilink-hub/ilink-hub; \
  sudo systemctl start ilink-hub'

# 4. 健康检查
curl -s http://43.138.149.34:8765/health   # → ok
```

## 提交前检查清单（部署相关）

- [ ] 已递增 `Cargo.toml` 的 `version`，并 `cargo build` 同步 `Cargo.lock`
- [ ] 本机调试用方案 2（brew），**绝不**裸拷到 `~/.local/bin`
- [ ] patch 对外用方案 1（`v*-mac`），minor/major 用完整 `release.yml`（`vX.Y.Z`）
- [ ] 远程 Hub 部署前已备份生产 SQLite 库

## 相关文档

- [部署安全加固](deployment-hardening.md)
- [环境变量配置](../api/configuration.md)
