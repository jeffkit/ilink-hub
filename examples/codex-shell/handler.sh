#!/usr/bin/env bash
#
# Codex Bridge Profile（Shell 版）
#
# 通过 Codex CLI 的 JSONL 输出模式（--json）接收用户消息，
# 支持多轮对话（exec resume <session_id>）。
#
# ─── 依赖 ──────────────────────────────────────────────────────────────────
#   必需：  codex CLI（已安装并认证：codex login）
#           jq（JSON 解析：brew install jq  或  sudo apt install jq）
#   可选：  python3（jq 不存在时的备用解析器）
#
# ─── 本地测试（不启动 bridge）──────────────────────────────────────────────
#   ILINK_MESSAGE="你好，介绍一下自己" \
#   ILINK_SESSION_ID="" \
#   ILINK_CWD="/path/to/your/project" \
#   bash handler.sh
#
# ─── 接入 bridge ────────────────────────────────────────────────────────────
#   ilink-hub-bridge --config profiles.yaml
#
# ───────────────────────────────────────────────────────────────────────────

set -euo pipefail

MESSAGE="${ILINK_MESSAGE:-}"
SESSION_ID="${ILINK_SESSION_ID:-}"
CODEX_BYPASS="--dangerously-bypass-approvals-and-sandbox"

# 切换工作目录（由 YAML cwd 字段注入，或从 ILINK_CWD 读取）
if [[ -n "${ILINK_CWD:-}" && -d "$ILINK_CWD" ]]; then
    cd "$ILINK_CWD"
fi

# 构造 codex 命令：有 session_id 时用 exec resume（多轮对话），否则新建会话
if [[ -n "$SESSION_ID" ]]; then
    CODEX_ARGS=(exec resume "$SESSION_ID" "$MESSAGE")
else
    CODEX_ARGS=(exec "$MESSAGE")
fi

# 执行 codex，关闭 stdin（echo ""），输出 JSONL 事件流
JSON_OUTPUT=$(echo "" | codex "${CODEX_ARGS[@]}" $CODEX_BYPASS --json 2>/dev/null)

# ── 解析 JSONL 输出 ──────────────────────────────────────────────────────────
NEW_SESSION_ID=""
RESPONSE=""

if command -v jq &>/dev/null; then
    # jq 路径：直接过滤 JSONL
    NEW_SESSION_ID=$(printf '%s\n' "$JSON_OUTPUT" \
        | jq -r 'select(.type=="thread.started") | .thread_id // empty' 2>/dev/null \
        | head -1)
    RESPONSE=$(printf '%s\n' "$JSON_OUTPUT" \
        | jq -r 'select(.type=="item.completed" and .item.type=="agent_message") | .item.text // empty' 2>/dev/null \
        | tr -d '\000')
elif command -v python3 &>/dev/null; then
    # Python3 备用路径
    PARSED=$(printf '%s\n' "$JSON_OUTPUT" | python3 - <<'PYEOF'
import json, sys
session_id = ""
parts = []
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get("type") == "thread.started" and not session_id:
            session_id = d.get("thread_id", "")
        elif d.get("type") == "item.completed":
            item = d.get("item", {})
            if item.get("type") == "agent_message":
                parts.append(item.get("text", ""))
    except json.JSONDecodeError:
        pass
print(session_id, end="\x01")
print("\n".join(parts), end="")
PYEOF
)
    NEW_SESSION_ID="${PARSED%%$'\x01'*}"
    RESPONSE="${PARSED#*$'\x01'}"
else
    >&2 echo "[codex-bridge] 警告：未找到 jq 或 python3，无法解析 JSON，直接输出原始内容"
    printf '%s' "$JSON_OUTPUT"
    exit 0
fi

# ── P0 协议输出（bridge 读取）────────────────────────────────────────────────
# 第 1 行：ILINK_SESSION:<uuid>（bridge 存入 Hub，下次调用时回注 ILINK_SESSION_ID）
# 其余行：Codex 的回复正文
if [[ -n "$NEW_SESSION_ID" ]]; then
    echo "ILINK_SESSION:$NEW_SESSION_ID"
fi
printf '%s' "$RESPONSE"
