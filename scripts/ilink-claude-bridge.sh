#!/usr/bin/env bash
# ilink-claude-bridge.sh
#
# 将 Claude Code CLI 接入 ilink-hub-bridge 的包装脚本。
# 负责「session 连续对话」：从 Hub 拿到当前 session UUID → --resume；
# 从 Claude 输出 JSON 里提取新的 session_id → 上报给 Hub。
#
# ─── 调用约定（由 ilink-hub-bridge 执行）─────────────────────────────────────
#
#   stdin:        用户消息文本（需在 YAML 里配置 stdin: message）
#   环境变量：
#     AGENT_SESSION_ID    当前 Hub session 对应的 Claude session UUID（空 = 新会话）
#     AGENT_SESSION_NAME  当前 Hub session 可读名称（如 "feature-a"，默认 "default"）
#     AGENT_CWD           工作目录（可选，覆盖脚本内默认值；推荐在 YAML cwd 里配置）
#     ANTHROPIC_API_KEY   API Key（若尚未 claude login；可选）
#     CLAUDE_EXTRA_ARGS   追加给 claude 的额外参数（空格分隔，可选）
#
# ─── 输出格式──────────────────────────────────────────────────────────────────
#
#   第 1 行：AGENT_SESSION:<session_uuid>   ← bridge 的 cli_session_first_line_prefix
#   其余行：Claude 的回复正文
#
# ─── 依赖─────────────────────────────────────────────────────────────────────
#
#   必需：  claude（已安装并在 PATH；已 `claude login` 或设置 ANTHROPIC_API_KEY）
#   可选：  jq（JSON 解析用，若无则回退到 Python3）
#           python3（jq 不存在时的备用解析器）
#
# ─── 使用方法──────────────────────────────────────────────────────────────────
#
#   1. 复制本脚本到合适位置（如 ~/scripts/ilink-claude-bridge.sh）
#   2. chmod +x ~/scripts/ilink-claude-bridge.sh
#   3. 在 YAML 里配置：
#        command: /path/to/ilink-claude-bridge.sh
#        stdin: message
#        cli_session_first_line_prefix: "AGENT_SESSION:"
#        env:
#          AGENT_SESSION_ID: "{{SESSION_ID}}"
#          AGENT_SESSION_NAME: "{{SESSION_NAME}}"
#          AGENT_CWD: "/path/to/your/project"
# ─────────────────────────────────────────────────────────────────────────────

set -euo pipefail

# ── 读取入参 ──────────────────────────────────────────────────────────────────
MESSAGE="$(cat)"                               # 从 stdin 读取用户消息
SESSION_ID="${AGENT_SESSION_ID:-}"             # Hub 存储的 Claude session UUID
SESSION_NAME="${AGENT_SESSION_NAME:-default}"  # Hub session 可读名称

# 工作目录：优先使用 AGENT_CWD，其次是脚本执行时的 cwd
if [[ -n "${AGENT_CWD:-}" && -d "$AGENT_CWD" ]]; then
    cd "$AGENT_CWD"
fi

# ── 构造 claude 参数 ──────────────────────────────────────────────────────────
CLAUDE_ARGS=("--output-format" "json" "--print" "$MESSAGE")

if [[ -n "$SESSION_ID" ]]; then
    CLAUDE_ARGS=("--resume" "$SESSION_ID" "${CLAUDE_ARGS[@]}")
fi

# 用户可通过 CLAUDE_EXTRA_ARGS 追加额外参数（如 --allowedTools "Bash,Read,Write"）
if [[ -n "${CLAUDE_EXTRA_ARGS:-}" ]]; then
    # shellcheck disable=SC2206
    CLAUDE_ARGS+=($CLAUDE_EXTRA_ARGS)
fi

# ── 执行 claude ───────────────────────────────────────────────────────────────
RAW_OUTPUT=""
CLAUDE_EXIT=0

RAW_OUTPUT=$(claude "${CLAUDE_ARGS[@]}" 2>&1) || CLAUDE_EXIT=$?

if [[ $CLAUDE_EXIT -ne 0 ]]; then
    # 若 --resume 失败（session 过期），降级为新会话重试
    if [[ -n "$SESSION_ID" ]]; then
        >&2 echo "[ilink-claude-bridge] session $SESSION_ID 无法 resume（exit $CLAUDE_EXIT），降级为新会话"
        FALLBACK_ARGS=("--output-format" "json" "--print" "$MESSAGE")
        if [[ -n "${CLAUDE_EXTRA_ARGS:-}" ]]; then
            # shellcheck disable=SC2206
            FALLBACK_ARGS+=($CLAUDE_EXTRA_ARGS)
        fi
        RAW_OUTPUT=$(claude "${FALLBACK_ARGS[@]}" 2>&1) || {
            echo "❌ Claude CLI 执行失败（降级新会话后仍失败）："
            echo "$RAW_OUTPUT"
            exit 1
        }
    else
        echo "❌ Claude CLI 执行失败："
        echo "$RAW_OUTPUT"
        exit 1
    fi
fi

# ── 解析 JSON 输出 ────────────────────────────────────────────────────────────
NEW_SESSION_ID=""
RESULT_TEXT=""

if command -v jq &>/dev/null; then
    # jq 路径
    NEW_SESSION_ID=$(printf '%s' "$RAW_OUTPUT" | jq -r '.session_id // empty' 2>/dev/null || true)
    RESULT_TEXT=$(printf '%s' "$RAW_OUTPUT" | jq -r '.result // empty' 2>/dev/null || true)
elif command -v python3 &>/dev/null; then
    # Python3 备用路径
    PARSED=$(printf '%s' "$RAW_OUTPUT" | python3 - <<'PYEOF'
import json, sys
try:
    d = json.loads(sys.stdin.read())
    print(d.get("session_id", ""), end="\x01")
    print(d.get("result", ""), end="")
except Exception:
    print("", end="\x01")
PYEOF
)
    NEW_SESSION_ID="${PARSED%%$'\x01'*}"
    RESULT_TEXT="${PARSED#*$'\x01'}"
else
    # 无 jq / python3，输出原始内容并退出
    >&2 echo "[ilink-claude-bridge] 警告：未找到 jq 或 python3，无法解析 JSON，直接输出原始内容"
    printf '%s' "$RAW_OUTPUT"
    exit 0
fi

# 若 JSON 解析失败（result 为空），回退到完整输出
if [[ -z "$RESULT_TEXT" ]]; then
    >&2 echo "[ilink-claude-bridge] 警告：JSON 解析无 result 字段，回退到原始输出"
    RESULT_TEXT="$RAW_OUTPUT"
fi

# ── 输出（bridge 读取第 1 行提取 session_id，其余作为回复正文）────────────────
if [[ -n "$NEW_SESSION_ID" ]]; then
    echo "AGENT_SESSION:$NEW_SESSION_ID"
fi
printf '%s' "$RESULT_TEXT"
