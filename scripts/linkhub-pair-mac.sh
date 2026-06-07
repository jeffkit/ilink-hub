#!/usr/bin/env bash
# Zero-config pairing: just run Hub. Relay connects automatically to ilinkhub.ai.
set -euo pipefail

HUB_CLIENT_URL="${HUB_CLIENT_URL:-http://127.0.0.1:8765}"

case "${1:-}" in
  hub|serve|"")
    export HUB_CLIENT_URL
    echo "Starting iLink Hub (zero-config pairing via ilinkhub.ai)..."
    echo "Clients: export WEIXIN_BASE_URL=$HUB_CLIENT_URL"
    exec cargo run -- serve
    ;;
  test-qr)
    curl -s "$HUB_CLIENT_URL/ilink/bot/get_bot_qrcode" | python3 -m json.tool
    ;;
  *)
    cat <<EOF
Usage:
  $0          # start Hub with automatic pairing relay
  $0 test-qr  # print QR pair URL from local Hub

No Tunely or tunnel token required.
Clients only need: WEIXIN_BASE_URL=$HUB_CLIENT_URL
EOF
    ;;
esac
