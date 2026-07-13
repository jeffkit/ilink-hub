#!/usr/bin/env bash
#
# Codex Bridge Profile（Shell 版）— AgentProc 0.3 NDJSON
#
# 从 stdin 读取一行 NDJSON turn 对象，调用 Codex CLI 的 JSONL 输出模式（--json），
# 支持多轮对话（exec resume <session_id>）与流式输出（partial 事件），
# 在 stdout 逐行输出 AgentProc 0.3 NDJSON 事件。
#
# ─── 依赖 ──────────────────────────────────────────────────────────────────
#   必需：  codex CLI（已安装并认证：codex login）
#           jq（JSON 解析与 NDJSON 编码：brew install jq）
#
# ─── 本地测试（不启动 bridge）──────────────────────────────────────────────
#   echo '{"type":"turn","message":"你好，介绍一下自己","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
#     | bash handler.sh
#
# ─── 接入 bridge ────────────────────────────────────────────────────────────
#   ilink-hub-bridge --config profiles.yaml
#
# ───────────────────────────────────────────────────────────────────────────

set -euo pipefail

CODEX_BYPASS="--dangerously-bypass-approvals-and-sandbox"

if ! command -v jq &>/dev/null; then
    printf '{"type":"error","message":"未找到 jq，请先安装（brew install jq 或 sudo apt install jq）"}\n'
    exit 1
fi

# ── 读取 stdin NDJSON turn ──────────────────────────────────────────────────
TURN=$(cat)
MESSAGE=$(printf '%s' "$TURN" | jq -r '.message // empty')
SESSION_ID=$(printf '%s' "$TURN" | jq -r '.session_id // empty')

# 切换工作目录（由 YAML cwd 字段注入）
if [[ -n "${AGENT_CWD:-}" && -d "$AGENT_CWD" ]]; then
    cd "$AGENT_CWD"
fi

# 构造 codex 命令：有 session_id 时用 exec resume（多轮对话），否则新建会话
if [[ -n "$SESSION_ID" ]]; then
    CODEX_ARGS=(exec resume "$SESSION_ID" "$MESSAGE")
else
    CODEX_ARGS=(exec "$MESSAGE")
fi

# ── 流式处理 JSONL 输出 ──────────────────────────────────────────────────────
# codex --json 逐行输出事件，边输出边解析，将 agent_message 通过 partial 事件实时推送给用户，
# 累积为最终 text 事件，thread.started 提取 session id。
#
# 事件类型说明：
#   thread.started                       → session id（保存，进程结束后输出 session 事件）
#   item.completed (agent_message)       → 回复文本（立即输出 partial 事件，并累积进 text）
NEW_SESSION_ID=""
FINAL_TEXT=""

emit_ndjson() {
    # $1 = jq filter producing an object; $2.. = jq args
    jq -nc "$@"
}

while IFS= read -r line; do
    [[ -z "$line" ]] && continue

    type=$(printf '%s' "$line" | jq -r '.type // empty' 2>/dev/null) || continue

    case "$type" in
        thread.started)
            NEW_SESSION_ID=$(printf '%s' "$line" | jq -r '.thread_id // empty' 2>/dev/null) || true
            ;;
        item.completed)
            item_type=$(printf '%s' "$line" | jq -r '.item.type // empty' 2>/dev/null) || continue
            if [[ "$item_type" == "agent_message" ]]; then
                text=$(printf '%s' "$line" | jq -r '.item.text // empty' 2>/dev/null) || continue
                if [[ -n "$text" ]]; then
                    # partial 事件：实时推送
                    jq -nc --arg t "$text" '{"type":"partial","text":$t}'
                    FINAL_TEXT="${FINAL_TEXT}${text}"
                fi
            fi
            ;;
    esac
done < <(echo "" | codex "${CODEX_ARGS[@]}" $CODEX_BYPASS --json 2>/dev/null)

# ── AgentProc 0.3 输出：text 事件（最终回复）+ session 事件 ─────────────────
if [[ -n "$FINAL_TEXT" ]]; then
    jq -nc --arg t "$FINAL_TEXT" '{"type":"text","text":$t}'
fi
if [[ -n "$NEW_SESSION_ID" ]]; then
    jq -nc --arg id "$NEW_SESSION_ID" '{"type":"session","id":$id}'
fi
