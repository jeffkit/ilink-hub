#!/bin/sh
# Install git hooks from scripts/hooks/ into .git/hooks/.
# Idempotent: safe to re-run after pulling new hook versions.

set -e

repo_root="$(git rev-parse --show-toplevel)"
hooks_src="$repo_root/scripts/hooks"
hooks_dst="$repo_root/.git/hooks"

if [ ! -d "$hooks_src" ]; then
    echo "✗ hooks source not found: $hooks_src"
    exit 1
fi

for hook in "$hooks_src"/*; do
    [ -f "$hook" ] || continue
    name="$(basename "$hook")"
    # Don't clobber user customizations of sample hooks
    case "$name" in
        *.sample) continue ;;
    esac
    cp "$hook" "$hooks_dst/$name"
    chmod +x "$hooks_dst/$name"
    echo "✓ installed $name"
done

echo ""
echo "Hooks installed. Verify with: ls -l .git/hooks/pre-commit"
