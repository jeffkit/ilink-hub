#!/usr/bin/env bash
# Deploy ilink-relay to tcloud_hk and switch nginx from Tunely to pairing relay.
set -euo pipefail

REMOTE="${REMOTE:-tcloud_hk}"
INSTALL_DIR="/opt/ilink-relay"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "==> Building release binary (linux x86_64)..."
cd "$ROOT"
cargo build --release --bin ilink-relay 2>/dev/null || true

# Cross-compile if local build is not linux
TARGET_BIN="$ROOT/target/release/ilink-relay"
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "==> Cross-compiling for x86_64-unknown-linux-gnu..."
  rustup target add x86_64-unknown-linux-gnu 2>/dev/null || true
  cargo build --release --target x86_64-unknown-linux-gnu --bin ilink-relay
  TARGET_BIN="$ROOT/target/x86_64-unknown-linux-gnu/release/ilink-relay"
fi

echo "==> Uploading to $REMOTE:$INSTALL_DIR"
ssh "$REMOTE" "sudo mkdir -p $INSTALL_DIR && sudo chown ubuntu:ubuntu $INSTALL_DIR"
scp "$TARGET_BIN" "$REMOTE:$INSTALL_DIR/ilink-relay"
scp "$ROOT/deploy/ilink-relay.service" "$REMOTE:/tmp/ilink-relay.service"
scp "$ROOT/deploy/nginx-ilinkhub-relay.conf" "$REMOTE:/tmp/ilinkhub.ai"

ssh "$REMOTE" bash -s <<'REMOTE'
set -euo pipefail
sudo mv /tmp/ilink-relay.service /etc/systemd/system/ilink-relay.service
sudo chmod +x /opt/ilink-relay/ilink-relay
sudo systemctl daemon-reload
sudo systemctl enable ilink-relay
sudo systemctl restart ilink-relay
sleep 1
curl -s http://127.0.0.1:8789/health

sudo mv /tmp/ilinkhub.ai /etc/nginx/sites-available/ilinkhub.ai
sudo ln -sf /etc/nginx/sites-available/ilinkhub.ai /etc/nginx/sites-enabled/ilinkhub.ai
sudo nginx -t && sudo systemctl reload nginx

# Stop Tunely (optional — pairing no longer needs it)
sudo systemctl disable --now linkhub-tunely 2>/dev/null || true

echo "==> HTTPS health:"
curl -s https://ilinkhub.ai/health
REMOTE

echo "Deploy complete."
