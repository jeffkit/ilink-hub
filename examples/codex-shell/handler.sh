#!/usr/bin/env bash
#
# Codex Bridge Profile（Shell 版）
#
# 通过 Codex CLI 的 JSONL 输出模式（--json）接收用户消息，
# 支持多轮对话（exec resume <session_id>）和流式输出（ILINK_PARTIAL）。
#
# ─── 依赖 ──────────────────────────────────────────────────────────────────
#   必需：  codex CLI（已安装并认证：codex login）
#           jq（JSON 解析与 ILINK_PARTIAL 编码：brew install jq）
#
# ─── 本地测试（不启动 bridge）──────────────────────────────────────────────
#   ILINK_MESSAGE="你好，介绍一下自己" \
#   ILINK_SESSION_ID="" \
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

if ! command -v jq &>/dev/null; then
    >&2 echo "[codex-bridge] 错误：未找到 jq，请先安装（brew install jq 或 sudo apt install jq）"
    exit 1
fi

# 构造 codex 命令：有 session_id 时用 exec resume（多轮对话），否则新建会话
if [[ -n "$SESSION_ID" ]]; then
    CODEX_ARGS=(exec resume "$SESSION_ID" "$MESSAGE")
else
    CODEX_ARGS=(exec "$MESSAGE")
fi

# ── 流式处理 JSONL 输出 ──────────────────────────────────────────────────────
# codex --json 逐行输出事件，边输出边解析，将 agent_message 通过 ILINK_PARTIAL 实时推送给用户。
#
# 事件类型说明：
#   thread.started          → session_id（保存，进程结束后输出 ILINK_SESSION:）
#   item.completed (agent_message) → 回复文本（立即输出 ILINK_PARTIAL:）
#
# ILINK_PARTIAL: 格式要求文本必须 JSON 编码（换行等特殊字符转义），用 jq -cn 完成。
#
NEW_SESSION_ID=""

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
                    # JSON-encode text so newlines and special chars are safely escaped on one line.
                    encoded=$(printf '%s' "$text" | jq -Rs '.')
                    printf 'ILINK_PARTIAL:%s\n' "$encoded"
                fi
            fi
            ;;
    esac
done < <(echo "" | codex "${CODEX_ARGS[@]}" $CODEX_BYPASS --json 2>/dev/null)

# ── P0 协议输出：ILINK_SESSION（仅在有 session_id 时输出）───────────────────
if [[ -n "$NEW_SESSION_ID" ]]; then
    printf 'ILINK_SESSION:%s\n' "$NEW_SESSION_ID"
fi
