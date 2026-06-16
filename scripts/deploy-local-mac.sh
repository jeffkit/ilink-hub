#!/usr/bin/env bash
# deploy-local-mac.sh
#
# 本地（macOS / Apple Silicon）部署 ilink-hub-bridge（及 ilink-hub）二进制，并重载
# launchd 服务。核心：拷贝新构建的二进制后**重新 ad-hoc 签名**，避免 AMFI 在运行时
# 以 OS_REASON_CODESIGNING 直接 SIGKILL 进程（“killed: 9”）。
#
# 背景：Rust 链接器只给二进制打一个轻量的 "linker-signed" 签名，拷贝/覆盖到目标路径后
# 该签名在 Apple Silicon 运行时常被 AMFI 拒绝。`codesign --force --sign -` 会重算页
# 哈希并写入标准 adhoc CodeDirectory，生成运行时可接受的签名。
#
# 用法：
#   scripts/deploy-local-mac.sh                 # 构建 + 安装 + 重签 + 重载 launchd
#   ILINK_BIN_DIR=~/bin scripts/deploy-local-mac.sh
#   ILINK_SKIP_BUILD=1 scripts/deploy-local-mac.sh   # 跳过 cargo build，仅安装/重签现有产物
#   ILINK_RELOAD=0 scripts/deploy-local-mac.sh       # 不重载 launchd
#
# 环境变量：
#   ILINK_BIN_DIR     安装目录（默认 ~/.local/bin）
#   ILINK_BINS        要安装的二进制（默认 "ilink-hub-bridge ilink-hub"）
#   ILINK_SKIP_BUILD  非空则跳过 cargo build
#   ILINK_RELOAD      0 则不重载 launchd（默认重载）
#   ILINK_LAUNCHD_LABELS  要 kickstart 的 launchd 标签
#                         （默认 "com.ilink-hub.ssh-tunnel com.ilink-hub.bridge-manager"）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BIN_DIR="${ILINK_BIN_DIR:-$HOME/.local/bin}"
BINS="${ILINK_BINS:-ilink-hub-bridge ilink-hub}"
LAUNCHD_LABELS="${ILINK_LAUNCHD_LABELS:-com.ilink-hub.ssh-tunnel com.ilink-hub.bridge-manager}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "本脚本仅用于 macOS 本地部署；其他平台请用 cargo install / 各自的部署流程。" >&2
  exit 1
fi

# ── 1. 构建 ────────────────────────────────────────────────────────────────────
if [[ -z "${ILINK_SKIP_BUILD:-}" ]]; then
  echo "==> cargo build --release (${BINS})"
  cd "$ROOT"
  for bin in $BINS; do
    cargo build --release --bin "$bin"
  done
else
  echo "==> 跳过构建（ILINK_SKIP_BUILD 已设置）"
fi

# ── 2. 安装 + 重签 ─────────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
for bin in $BINS; do
  src="$ROOT/target/release/$bin"
  dst="$BIN_DIR/$bin"
  if [[ ! -x "$src" ]]; then
    echo "!! 未找到构建产物：$src（先构建或检查 ILINK_BINS）" >&2
    exit 1
  fi

  echo "==> 安装 $bin -> $dst"
  # 用临时文件 + mv 原子替换，避免覆盖正在运行的二进制导致 cdhash 不一致
  tmp="$(mktemp "$BIN_DIR/.$bin.XXXXXX")"
  cp -f "$src" "$tmp"

  echo "    清理扩展属性（quarantine / provenance）"
  xattr -cr "$tmp" 2>/dev/null || true

  echo "    重新 ad-hoc 签名（修复 AMFI 运行时拒签 / OS_REASON_CODESIGNING）"
  codesign --remove-signature "$tmp" 2>/dev/null || true
  codesign --force --sign - "$tmp"
  codesign -v "$tmp"

  chmod +x "$tmp"
  mv -f "$tmp" "$dst"
  echo "    OK: $(codesign -dvvv "$dst" 2>&1 | awk -F= '/^Signature/{print $2}')"
done

# ── 3. 重载 launchd ────────────────────────────────────────────────────────────
if [[ "${ILINK_RELOAD:-1}" != "0" ]]; then
  uid="$(id -u)"
  for label in $LAUNCHD_LABELS; do
    if launchctl print "gui/$uid/$label" >/dev/null 2>&1; then
      echo "==> 重启 launchd 服务：$label"
      launchctl kickstart -k "gui/$uid/$label" || \
        echo "   (kickstart 失败，可手动 launchctl bootout/bootstrap $label)" >&2
    else
      echo "==> 跳过 $label（未在 launchd 中加载）"
    fi
  done

  echo "==> launchd 状态："
  launchctl list | grep -E "$(echo "$LAUNCHD_LABELS" | tr ' ' '|')" || true
fi

echo "本地部署完成。"
