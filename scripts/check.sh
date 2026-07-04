#!/bin/sh
# Pre-commit style checks. Mirrors the CI fmt + clippy steps so failures
# are caught locally before push.
#
# Usage:
#   scripts/check.sh           # check only
#   scripts/check.sh --fix     # auto-fix fmt, then re-check
#
# Exits 0 on success, 1 on any failure.

set -e

# Source cargo env if not already in PATH (CI runners, fresh shells, etc.)
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

run_fmt() {
    echo "→ cargo fmt --all -- --check"
    cargo fmt --all -- --check
}

run_clippy() {
    echo "→ cargo clippy --all-targets --all-features -- -D warnings"
    cargo clippy --all-targets --all-features -- -D warnings
}

# Auto-fix fmt first when requested
if [ "${1:-}" = "--fix" ]; then
    echo "→ cargo fmt --all (auto-fix)"
    cargo fmt --all
fi

run_fmt
run_clippy

echo "✓ check.sh passed"
