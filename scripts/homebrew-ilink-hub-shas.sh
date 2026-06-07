#!/usr/bin/env bash
# 在 GitHub Release v$VERSION 已发布且资产已上传后，打印 macOS 四个文件的 sha256，
# 供更新 jeffkit/homebrew-tap Formula/ilink-hub.rb。
set -euo pipefail
VERSION="${1:?用法: $0 <版本号，不含 v，例如 0.1.8>}"
BASE="https://github.com/jeffkit/ilink-hub/releases/download/v${VERSION}"
FILES=(
  ilink-hub-macos-aarch64
  ilink-hub-macos-x86_64
  ilink-hub-bridge-macos-aarch64
  ilink-hub-bridge-macos-x86_64
)
echo "# v${VERSION} — 将下列 sha256 填入 homebrew-tap Formula/ilink-hub.rb"
for f in "${FILES[@]}"; do
  url="${BASE}/${f}"
  echo "fetching $f ..."
  sum="$(curl -fsSL "$url" | shasum -a 256 | awk '{print $1}')"
  printf '%-42s %s\n' "$f" "$sum"
done
