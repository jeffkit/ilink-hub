# Bridge Profile 完整示例

> 最后更新：2026-07-13

本页提供**开箱即用**的 Bridge Profile 示例，均已通过本地运行验证：

| 示例 | 语言 | AI 工具 | 目录 |
|------|------|---------|------|
| [Codex（Shell）](#codex-shell) | Shell | OpenAI Codex CLI | `examples/codex-shell/` |

所有示例均：
- 通过 **AgentProc 0.3 NDJSON 协议**接入（stdin 读 turn，stdout 写事件）
- 支持**多轮对话**（session resume）
- 可在**不启动 bridge** 的情况下单独测试

---

## Codex（Shell）{#codex-shell}

用纯 Shell 脚本调用 OpenAI Codex CLI，从 stdin 读 NDJSON turn，解析 `--json` 事件流后在 stdout 输出 NDJSON 事件。

### 前提条件

```bash
codex --version         # Codex CLI 已安装
codex login             # 或 export OPENAI_API_KEY=sk-...
jq --version            # brew install jq  或  sudo apt install jq
```

### 测试

```bash
cd examples/codex-shell

# 本地模拟调用（不需要启动 bridge）：向 stdin 写一行 turn NDJSON
echo '{"type":"turn","message":"你好，介绍一下自己","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | AGENT_CWD="$(pwd)" bash handler.sh
```

预期输出（NDJSON 事件流）：

```
{"type":"partial","text":"你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？"}
{"type":"text","text":"你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？"}
{"type":"session","id":"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"}
```

### 接入 Bridge

修改 `profiles.yaml` 中的 `cwd` 为你的项目目录，然后：

```bash
ilink-hub-bridge --config profiles.yaml
```

### 核心代码

```bash
# examples/codex-shell/handler.sh
TURN=$(cat)                       # 读 stdin NDJSON turn
MESSAGE=$(printf '%s' "$TURN" | jq -r '.message // empty')
SESSION_ID=$(printf '%s' "$TURN" | jq -r '.session_id // empty')

# 有 session_id 时用 exec resume（多轮对话），否则新建会话
if [[ -n "$SESSION_ID" ]]; then
    CODEX_ARGS=(exec resume "$SESSION_ID" "$MESSAGE")
else
    CODEX_ARGS=(exec "$MESSAGE")
fi

NEW_SESSION_ID=""
FINAL_TEXT=""

# 解析 codex --json 事件流，发 partial 事件并累积 text
while IFS= read -r line; do
    type=$(printf '%s' "$line" | jq -r '.type // empty')
    case "$type" in
        thread.started)
            NEW_SESSION_ID=$(printf '%s' "$line" | jq -r '.thread_id // empty')
            ;;
        item.completed)
            item_type=$(printf '%s' "$line" | jq -r '.item.type // empty')
            if [[ "$item_type" == "agent_message" ]]; then
                text=$(printf '%s' "$line" | jq -r '.item.text // empty')
                [[ -n "$text" ]] && {
                    jq -nc --arg t "$text" '{"type":"partial","text":$t}'
                    FINAL_TEXT="${FINAL_TEXT}${text}"
                }
            fi
            ;;
    esac
done < <(echo "" | codex "${CODEX_ARGS[@]}" \
    --dangerously-bypass-approvals-and-sandbox --json 2>/dev/null)

# AgentProc 0.3 输出：text 事件 + session 事件
[[ -n "$FINAL_TEXT" ]] && jq -nc --arg t "$FINAL_TEXT" '{"type":"text","text":$t}'
[[ -n "$NEW_SESSION_ID" ]] && jq -nc --arg id "$NEW_SESSION_ID" '{"type":"session","id":$id}'
```

**Codex JSON 事件流格式：**

```jsonl
{"type":"thread.started","thread_id":"019eac60-..."}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"你好！..."}}
{"type":"turn.completed","usage":{...}}
```

**关键设计点：**
- `echo ""` 关闭 codex stdin，避免 codex 等待管道输入
- `--json` 输出 JSONL 格式，便于 `jq` 精确提取
- `--dangerously-bypass-approvals-and-sandbox` 用于非交互环境（仅在受信任目录使用）
- 输出统一为 AgentProc 0.3 NDJSON 事件：`partial`（实时分块）+ `text`（最终回复）+ `session`（session id）

---

## 多轮对话验证

```bash
# 第一轮：获取 session_id
echo '{"type":"turn","message":"用一句话说你好","session_id":"","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | bash handler.sh
# 输出：{"type":"partial","text":"你好。"}
#       {"type":"text","text":"你好。"}
#       {"type":"session","id":"019eac6a-..."}

# 第二轮：用上一轮的 session_id 继续对话
echo '{"type":"turn","message":"我上一条消息说了什么？","session_id":"019eac6a-...","from_user":"test","protocol_version":"0.3","session_name":"default","attachments":[]}' \
  | bash handler.sh
# 输出：{"type":"partial","text":"你上一条消息是：\"用一句话说你好\"。"}
#       {"type":"text","text":"你上一条消息是：\"用一句话说你好\"。"}
#       {"type":"session","id":"019eac6a-..."}
```

---

## 下一步

- [Profile 协议规范（AgentProc 0.3）](/bridge/profile-spec) — 了解 stdin/stdout NDJSON 约定的完整定义
- [Node.js 开发教程](/bridge/develop-nodejs) — 从零实现自定义 handler
- [Python 开发教程](/bridge/develop-python) — Python 版本教程
- [使用指引](/bridge/USAGE) — 多 CLI 配置、多项目管理
