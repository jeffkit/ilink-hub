# Bridge Profile 完整示例

> 最后更新：2026-06-26

本页提供三个**开箱即用**的 Bridge Profile 示例，均已通过本地运行验证：

| 示例 | 语言 | AI 工具 | 目录 |
|------|------|---------|------|
| [Codex（Shell）](#codex-shell) | Shell | OpenAI Codex CLI | `examples/codex-shell/` |

所有示例均：
- 通过 bridge SDK（`agentproc`）或标准 P0 协议接入
- 支持**多轮对话**（session resume）
- 可在**不启动 bridge** 的情况下单独测试

---

## Codex（Shell）{#codex-shell}

用纯 Shell 脚本调用 OpenAI Codex CLI，通过 `--json` 事件流解析回复和 session_id。

### 前提条件

```bash
codex --version         # Codex CLI 已安装
codex login             # 或 export OPENAI_API_KEY=sk-...
jq --version            # brew install jq  或  sudo apt install jq
```

### 测试

```bash
cd examples/codex-shell

# 本地模拟调用（不需要启动 bridge）
AGENT_MESSAGE="你好，介绍一下自己" \
AGENT_SESSION_ID="" \
AGENT_CWD="$(pwd)" \
bash handler.sh
```

预期输出：

```
AGENT_SESSION:xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
你好！我是 Codex，一个 AI 编程助手。有什么可以帮你的吗？
```

### 接入 Bridge

修改 `profiles.yaml` 中的 `cwd` 为你的项目目录，然后：

```bash
ilink-hub-bridge --config profiles.yaml
```

### 核心代码

```bash
# examples/codex-shell/handler.sh
MESSAGE="${AGENT_MESSAGE:-}"
SESSION_ID="${AGENT_SESSION_ID:-}"

# 有 session_id 时用 exec resume（多轮对话），否则新建会话
if [[ -n "$SESSION_ID" ]]; then
    CODEX_ARGS=(exec resume "$SESSION_ID" "$MESSAGE")
else
    CODEX_ARGS=(exec "$MESSAGE")
fi

# 关闭 stdin（echo ""），使用 --json 获取结构化输出
JSON_OUTPUT=$(echo "" | codex "${CODEX_ARGS[@]}" \
    --dangerously-bypass-approvals-and-sandbox --json 2>/dev/null)

# 提取 session_id 和回复文本
NEW_SESSION_ID=$(printf '%s\n' "$JSON_OUTPUT" \
    | jq -r 'select(.type=="thread.started") | .thread_id // empty' | head -1)
RESPONSE=$(printf '%s\n' "$JSON_OUTPUT" \
    | jq -r 'select(.type=="item.completed" and .item.type=="agent_message") | .item.text // empty')

# P0 输出：第一行为 AGENT_SESSION:<uuid>，其余为回复正文
if [[ -n "$NEW_SESSION_ID" ]]; then echo "AGENT_SESSION:$NEW_SESSION_ID"; fi
printf '%s' "$RESPONSE"
```

**Codex JSON 事件流格式：**

```jsonl
{"type":"thread.started","thread_id":"019eac60-..."}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"你好！..."}}
{"type":"turn.completed","usage":{...}}
```

**关键设计点：**
- `echo ""` 关闭 stdin，避免 codex 等待管道输入
- `--json` 输出 JSONL 格式，便于 `jq` 精确提取
- `--dangerously-bypass-approvals-and-sandbox` 用于非交互环境（仅在受信任目录使用）
- 脚本同时支持 `jq` 和 `python3` 两种解析方式

---

## 多轮对话验证

三个示例均已通过多轮对话测试：

```bash
# 第一轮：获取 session_id
AGENT_MESSAGE="用一句话说你好" AGENT_SESSION_ID="" bash handler.sh
# 输出：AGENT_SESSION:019eac6a-...
#       你好。

# 第二轮：用上一轮的 session_id 继续对话
AGENT_MESSAGE="我上一条消息说了什么？" AGENT_SESSION_ID="019eac6a-..." bash handler.sh
# 输出：AGENT_SESSION:019eac6a-...
#       你上一条消息是："用一句话说你好"。
```

---

## 下一步

- [Profile 协议规范（P0）](/bridge/profile-spec) — 了解 stdin/stdout 约定的完整定义
- [Node.js 开发教程](/bridge/develop-nodejs) — 从零实现自定义 handler
- [Python 开发教程](/bridge/develop-python) — Python 版本教程
- [使用指引](/bridge/USAGE) — 多 CLI 配置、多项目管理
