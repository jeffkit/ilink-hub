#!/usr/bin/env bash
# deploy-local-brew.sh — 方案 2：本地构建 → 通过 brew 安装到 /opt/homebrew/bin，零 CI。
#
# 用途：本机快速调试 patch 级改动。**产物仅在本机**，不上传 GitHub，公共 tap 不变。
# 与 deploy-local-mac.sh 的区别：不再把二进制裸拷到 ~/.local/bin，而是经 brew 管理
# （/opt/homebrew/bin），因此 launchd / brew services 指向的 brew 路径会拿到新二进制。
#
# 流程：
#   1. 本地 cargo build --release（host arch）出 ilink-hub + ilink-hub-bridge
#   2. ad-hoc 重签（避免 macOS AMFI 运行时 SIGKILL / "killed: 9"）
#   3. 临时把 jeffkit/tap 的 formula 改成 file:// 指向本地产物 + 本地 sha256
#   4. brew reinstall（从本地文件装到 Cellar，并 relink /opt/homebrew/bin）
#   5. 还原 tap formula（git checkout），保持 tap 干净
#
# 用法：
#   scripts/deploy-local-brew.sh                          # 用 Cargo.toml 的版本，默认重载 launchd
#   ILINK_NO_RELOAD_LAUNCHD=1 scripts/deploy-local-brew.sh  # 跳过重载（仅换二进制，不动服务）
#
# 注意：这是「本机调试」路径。要让其它机器 / 公共 tap 拿到新版本，请用方案 1
# （打 v*-mac tag 触发 release-mac-fast.yml）或完整 release（打 vX.Y.Z tag）。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "本脚本仅用于 macOS 本地 brew 部署。" >&2
  exit 1
fi

VERSION="$(grep '^version' Cargo.toml | head -1 | sed 's/.*= *"\(.*\)"/\1/')"
TAP_DIR="$(brew --repository jeffkit/tap 2>/dev/null || true)"
FORMULA="$TAP_DIR/Formula/ilink-hub.rb"

if [[ -z "$TAP_DIR" || ! -f "$FORMULA" ]]; then
  echo "!! 未找到 jeffkit/tap formula（$FORMULA）。先执行：brew tap jeffkit/tap" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  arm64) SUFFIX="macos-aarch64" ;;
  x86_64) SUFFIX="macos-x86_64" ;;
  *) echo "!! 不支持的架构：$ARCH" >&2; exit 1 ;;
esac

echo "==> 本地构建 release（ilink-hub + ilink-hub-bridge）v$VERSION ($ARCH)"
cargo build --release --bin ilink-hub --bin ilink-hub-bridge

STAGE="$(mktemp -d)"
HUB_ASSET="ilink-hub-$SUFFIX"
BRIDGE_ASSET="ilink-hub-bridge-$SUFFIX"
cp target/release/ilink-hub "$STAGE/$HUB_ASSET"
cp target/release/ilink-hub-bridge "$STAGE/$BRIDGE_ASSET"

echo "==> ad-hoc 重签（修复 AMFI 运行时拒签）"
for f in "$STAGE/$HUB_ASSET" "$STAGE/$BRIDGE_ASSET"; do
  xattr -cr "$f" 2>/dev/null || true
  codesign --remove-signature "$f" 2>/dev/null || true
  codesign --force --sign - "$f"
done

HUB_SHA="$(shasum -a 256 "$STAGE/$HUB_ASSET" | awk '{print $1}')"
BRIDGE_SHA="$(shasum -a 256 "$STAGE/$BRIDGE_ASSET" | awk '{print $1}')"

BACKUP="$(mktemp)"
cp "$FORMULA" "$BACKUP"
cleanup() {
  # 还原 tap formula（优先 git，保持 tap 干净），清理临时文件
  if ! git -C "$TAP_DIR" checkout -- Formula/ilink-hub.rb 2>/dev/null; then
    cp "$BACKUP" "$FORMULA"
  fi
  rm -f "$BACKUP"
  rm -rf "$STAGE"
}
trap cleanup EXIT

echo "==> 临时改写 tap formula 指向本地文件"
cat > "$FORMULA" <<RUBY
# typed: false
# frozen_string_literal: true
# !! 本地调试覆盖（deploy-local-brew.sh 生成）—— 切勿提交；脚本结束会自动还原。
class IlinkHub < Formula
  desc "iLink-compatible multiplexer hub (local dev build)"
  homepage "https://jeffkit.github.io/ilink-hub/"
  version "$VERSION"
  license "MIT"

  on_macos do
    url "file://$STAGE/$HUB_ASSET"
    sha256 "$HUB_SHA"

    resource "ilink_hub_bridge" do
      url "file://$STAGE/$BRIDGE_ASSET"
      sha256 "$BRIDGE_SHA"
    end
  end

  def install
    bin.install "$HUB_ASSET" => "ilink-hub"
    resource("ilink_hub_bridge").stage do
      bin.install "$BRIDGE_ASSET" => "ilink-hub-bridge"
    end
  end

  test do
    assert_match "ilink-hub", shell_output("#{bin}/ilink-hub --version")
  end
end
RUBY

echo "==> brew reinstall（从本地文件）"
brew reinstall --formula "$FORMULA"

# 二次重签 Cellar 内的二进制，确保 brew 拷贝/重定位后签名仍有效
CELLAR="$(brew --cellar ilink-hub 2>/dev/null)/$VERSION/bin"
if [[ -d "$CELLAR" ]]; then
  for f in ilink-hub ilink-hub-bridge; do
    codesign --force --sign - "$CELLAR/$f" 2>/dev/null || true
  done
fi

echo "==> 安装结果"
/opt/homebrew/bin/ilink-hub --version || true
/opt/homebrew/bin/ilink-hub-bridge --version || true

if [[ "${ILINK_NO_RELOAD_LAUNCHD:-0}" != "1" ]]; then
  uid="$(id -u)"
  SERVICE="com.ilink-hub.bridge-manager"
  PLIST="$HOME/Library/LaunchAgents/$SERVICE.plist"
  if [[ -f "$PLIST" ]]; then
    echo "==> 重载 bridge-manager launchd（kickstart 热换二进制，不卸载服务）"
    # 先尝试 kickstart（服务已加载时热重启，不需要先 bootout）。
    # 只有在服务完全未加载时才走 bootout+bootstrap，并在失败后回退到手动启动，
    # 避免 bootstrap 失败导致服务彻底消失。
    if launchctl kickstart -k "gui/$uid/$SERVICE" 2>/dev/null; then
      echo "    kickstart 成功"
    else
      echo "    服务未加载，尝试 bootstrap..."
      launchctl bootout "gui/$uid/$SERVICE" 2>/dev/null || true
      sleep 1
      if ! launchctl bootstrap "gui/$uid" "$PLIST" 2>/dev/null; then
        echo "    bootstrap 失败，直接启动 bridge-manager..."
        nohup /opt/homebrew/bin/ilink-hub-bridge manager \
          >> "$HOME/ilink-logs/bridge-manager.log" \
          2>> "$HOME/ilink-logs/bridge-manager-error.log" &
      fi
    fi
    echo "    等待 3s 后查看进程"
    sleep 3
    pgrep -fl "ilink-hub-bridge manager" || echo "!! bridge-manager 未起来，请检查 ~/ilink-logs/bridge-manager-error.log"
  else
    echo "!! 未找到 $PLIST，跳过 launchd 重载" >&2
  fi
else
  echo "==> ILINK_NO_RELOAD_LAUNCHD=1，跳过 launchd 重载（bridge-manager 维持原状态）"
fi

echo "本地 brew 部署完成 v${VERSION} 。tap formula 已还原（保持 jeffkit/tap 干净）。"
